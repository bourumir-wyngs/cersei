//! cersei-agent: The high-level Agent API with builder pattern, agentic loop,
//! realtime event streaming, broadcast channels, and reporters.

pub mod agent_tool;
pub mod auto_dream;
pub mod compact;
pub mod context_analyzer;
pub mod coordinator;
pub mod effort;
pub mod events;
pub mod reporters;
mod runner;
pub mod session_memory;
pub mod system_prompt;

// Re-export runner utilities
pub use runner::apply_tool_result_budget;
pub use runner::strip_thinking_blocks;

use cersei_hooks::Hook;
use cersei_mcp::McpServerConfig;
use cersei_memory::Memory;
use cersei_provider::Provider;
use cersei_tools::network_policy::NetworkPolicy;
use cersei_tools::permissions::{AllowAll, PermissionPolicy};
use cersei_tools::{CostTracker, Extensions, Tool};
use cersei_types::*;
use events::AgentEvent;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

// Re-exports
pub use events::{AgentStream, CompactReason, WarningState};
pub use reporters::Reporter;

pub(crate) const DEFAULT_TOOL_RESULT_BUDGET_CHARS: usize = 300_000;

// ─── Agent output ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AgentOutput {
    pub message: Message,
    pub usage: Usage,
    pub stop_reason: StopReason,
    pub turns: u32,
    pub tool_calls: Vec<ToolCallRecord>,
}

impl AgentOutput {
    pub fn text(&self) -> &str {
        self.message.get_text().unwrap_or("")
    }
}

#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub name: String,
    pub id: String,
    pub input: serde_json::Value,
    pub result: String,
    pub is_error: bool,
    pub duration: Duration,
}

// ─── Agent ───────────────────────────────────────────────────────────────────

#[allow(dead_code)]
pub struct Agent {
    provider: Box<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    system_prompt: Option<String>,
    append_system_prompt: Option<String>,
    model: Option<String>,
    max_turns: u32,
    max_tokens: u32,
    temperature: Option<f32>,
    thinking_budget: Option<u32>,
    working_dir: PathBuf,
    permission_policy: Arc<dyn PermissionPolicy>,
    network_policy: Option<Arc<dyn NetworkPolicy>>,
    memory: Option<Arc<dyn Memory>>,
    session_id: Option<String>,
    hooks: Vec<Arc<dyn Hook>>,
    mcp_manager: Option<Arc<cersei_mcp::McpManager>>,
    event_handler: Option<Box<dyn Fn(&AgentEvent) + Send + Sync>>,
    broadcast_tx: Option<broadcast::Sender<AgentEvent>>,
    reporters: Vec<Arc<dyn Reporter>>,
    event_filter: Option<Box<dyn Fn(&AgentEvent) -> bool + Send + Sync>>,
    cost_tracker: Arc<CostTracker>,
    auto_compact: bool,
    compact_threshold: f64,
    tool_result_budget: usize,
    messages: Arc<parking_lot::Mutex<Vec<Message>>>,
    cumulative_usage: Arc<parking_lot::Mutex<Usage>>,
    cancel_token: tokio_util::sync::CancellationToken,
    tool_extensions: Extensions,
}

fn duplicate_tool_names(tools: &[Box<dyn Tool>]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut duplicates = std::collections::HashSet::new();
    let mut ordered = Vec::new();

    for tool in tools {
        let name = tool.name().to_string();
        if !seen.insert(name.clone()) && duplicates.insert(name.clone()) {
            ordered.push(name);
        }
    }

    ordered
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    /// Run a prompt through the agentic loop.
    pub async fn run(&self, prompt: &str) -> cersei_types::Result<AgentOutput> {
        runner::run_agent(self, prompt).await
    }

    /// Run with streaming — returns a stream of AgentEvents.
    pub fn run_stream(&self, prompt: &str) -> AgentStream {
        let (event_tx, event_rx) = mpsc::channel(512);
        let (control_tx, control_rx) = mpsc::channel(64);

        let prompt = prompt.to_string();
        let agent_ptr = unsafe {
            // SAFETY: Agent is borrowed for the duration of the spawned task.
            // In a real implementation, Agent would be Arc-wrapped.
            &*(self as *const Agent)
        };

        tokio::spawn(async move {
            let result =
                runner::run_agent_streaming(agent_ptr, &prompt, event_tx.clone(), control_rx).await;
            match result {
                Ok(output) => {
                    let _ = event_tx.send(AgentEvent::Complete(output)).await;
                }
                Err(e) => {
                    let _ = event_tx.send(AgentEvent::Error(e.to_string())).await;
                }
            }
        });

        AgentStream::new(event_rx, control_tx)
    }

    /// Multi-turn: send a follow-up message in the same conversation.
    pub async fn reply(&self, message: &str) -> cersei_types::Result<AgentOutput> {
        runner::run_agent(self, message).await
    }

    /// Access the conversation history.
    pub fn messages(&self) -> Vec<Message> {
        self.messages.lock().clone()
    }

    /// Clear the conversation history.
    pub fn clear_messages(&self) {
        self.messages.lock().clear();
    }

    /// Replace the conversation history.
    pub fn set_messages(&self, msgs: Vec<Message>) {
        *self.messages.lock() = msgs;
    }

    /// Manually compact the conversation by summarizing it via an LLM call.
    /// Returns (messages_before, messages_after, tokens_freed_estimate) on success.
    pub async fn compact_now(&self) -> cersei_types::Result<crate::compact::CompactResult> {
        let messages = self.messages.lock().clone();
        let model = self.model.as_deref().unwrap_or("unknown");

        // When fewer messages than keep_recent, summarize everything (keep 0 recent).
        let keep = if messages.len() <= crate::compact::KEEP_RECENT_MESSAGES {
            0
        } else {
            crate::compact::KEEP_RECENT_MESSAGES
        };

        let result = crate::compact::compact_conversation(
            self.provider.as_ref(),
            &messages,
            model,
            keep,
            None,
        )
        .await?;

        // Replace messages with summary + recent
        let split_idx = messages.len().saturating_sub(keep);
        let recent = messages[split_idx..].to_vec();
        let summary_msg = cersei_types::Message::user(result.summary.clone());
        let mut new_messages = vec![summary_msg];
        new_messages.extend(recent);

        // Strip orphaned tool results: tool results whose matching tool_use
        // was in the compacted portion and no longer exists.
        let tool_use_ids: std::collections::HashSet<String> = new_messages
            .iter()
            .flat_map(|m| m.content_blocks())
            .filter_map(|b| {
                if let cersei_types::ContentBlock::ToolUse { id, .. } = &b {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect();

        for msg in &mut new_messages {
            if let cersei_types::MessageContent::Blocks(blocks) = &mut msg.content {
                blocks.retain(|b| {
                    if let cersei_types::ContentBlock::ToolResult { tool_use_id, .. } = b {
                        tool_use_ids.contains(tool_use_id)
                    } else {
                        true
                    }
                });
            }
        }
        // Remove messages that became empty after stripping
        new_messages.retain(|m| !m.content_blocks().is_empty());

        *self.messages.lock() = new_messages;

        // Reset cumulative usage so the prompt token count reflects the compacted context.
        *self.cumulative_usage.lock() = Usage::default();

        Ok(result)
    }

    /// Get cumulative usage/cost.
    pub fn usage(&self) -> Usage {
        self.cumulative_usage.lock().clone()
    }

    /// Cancel a running agent.
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    /// Subscribe to the broadcast channel (requires enable_broadcast on builder).
    pub fn subscribe(&self) -> Option<broadcast::Receiver<AgentEvent>> {
        self.broadcast_tx.as_ref().map(|tx| tx.subscribe())
    }

    /// Emit an event to all listeners.
    pub(crate) fn emit(&self, event: AgentEvent) {
        // Apply filter
        if let Some(filter) = &self.event_filter {
            if !filter(&event) {
                return;
            }
        }

        // Callback handler
        if let Some(handler) = &self.event_handler {
            handler(&event);
        }

        // Broadcast channel
        if let Some(tx) = &self.broadcast_tx {
            let _ = tx.send(event.clone());
        }

        // Reporters
        for reporter in &self.reporters {
            let reporter = Arc::clone(reporter);
            let event = event.clone();
            tokio::spawn(async move {
                reporter.on_event(&event).await;
            });
        }
    }
}

// ─── Agent builder ───────────────────────────────────────────────────────────

pub struct AgentBuilder {
    provider: Option<Box<dyn Provider>>,
    tools: Vec<Box<dyn Tool>>,
    system_prompt: Option<String>,
    append_system_prompt: Option<String>,
    model: Option<String>,
    max_turns: u32,
    max_tokens: u32,
    temperature: Option<f32>,
    thinking_budget: Option<u32>,
    working_dir: Option<PathBuf>,
    permission_policy: Option<Arc<dyn PermissionPolicy>>,
    network_policy: Option<Arc<dyn NetworkPolicy>>,
    memory: Option<Arc<dyn Memory>>,
    session_id: Option<String>,
    hooks: Vec<Arc<dyn Hook>>,
    mcp_servers: Vec<McpServerConfig>,
    event_handler: Option<Box<dyn Fn(&AgentEvent) + Send + Sync>>,
    broadcast_capacity: Option<usize>,
    reporters: Vec<Arc<dyn Reporter>>,
    event_filter: Option<Box<dyn Fn(&AgentEvent) -> bool + Send + Sync>>,
    cancel_token: Option<tokio_util::sync::CancellationToken>,
    auto_compact: bool,
    compact_threshold: f64,
    tool_result_budget: usize,
    initial_messages: Option<Vec<Message>>,
    tool_extensions: Extensions,
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self {
            provider: None,
            tools: Vec::new(),
            system_prompt: None,
            append_system_prompt: None,
            model: None,
            max_turns: 50,
            max_tokens: 16384,
            temperature: None,
            thinking_budget: None,
            working_dir: None,
            permission_policy: None,
            network_policy: None,
            memory: None,
            session_id: None,
            hooks: Vec::new(),
            mcp_servers: Vec::new(),
            event_handler: None,
            broadcast_capacity: None,
            reporters: Vec::new(),
            event_filter: None,
            cancel_token: None,
            auto_compact: true,
            compact_threshold: 0.9,
            tool_result_budget: DEFAULT_TOOL_RESULT_BUDGET_CHARS,
            initial_messages: None,
            tool_extensions: Extensions::default(),
        }
    }
}

impl AgentBuilder {
    pub fn provider(mut self, p: impl Provider + 'static) -> Self {
        self.provider = Some(Box::new(p));
        self
    }

    pub fn tool(mut self, t: impl Tool + 'static) -> Self {
        self.tools.push(Box::new(t));
        self
    }

    pub fn tools(mut self, ts: Vec<Box<dyn Tool>>) -> Self {
        self.tools.extend(ts);
        self
    }

    pub fn system_prompt(mut self, s: impl Into<String>) -> Self {
        self.system_prompt = Some(s.into());
        self
    }

    pub fn append_system_prompt(mut self, s: impl Into<String>) -> Self {
        self.append_system_prompt = Some(s.into());
        self
    }

    pub fn model(mut self, m: impl Into<String>) -> Self {
        self.model = Some(m.into());
        self
    }

    pub fn max_turns(mut self, n: u32) -> Self {
        self.max_turns = n;
        self
    }

    pub fn max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    pub fn temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }

    pub fn thinking_budget(mut self, tokens: u32) -> Self {
        self.thinking_budget = Some(tokens);
        self
    }

    pub fn working_dir(mut self, p: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(p.into());
        self
    }

    pub fn permission_policy(mut self, p: impl PermissionPolicy + 'static) -> Self {
        self.permission_policy = Some(Arc::new(p));
        self
    }

    pub fn network_policy(mut self, p: impl NetworkPolicy + 'static) -> Self {
        self.network_policy = Some(Arc::new(p));
        self
    }

    pub fn memory(mut self, m: impl Memory + 'static) -> Self {
        self.memory = Some(Arc::new(m));
        self
    }

    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }

    pub fn hook(mut self, h: impl Hook + 'static) -> Self {
        self.hooks.push(Arc::new(h));
        self
    }

    pub fn mcp_server(mut self, config: McpServerConfig) -> Self {
        self.mcp_servers.push(config);
        self
    }

    pub fn on_event(mut self, f: impl Fn(&AgentEvent) + Send + Sync + 'static) -> Self {
        self.event_handler = Some(Box::new(f));
        self
    }

    pub fn enable_broadcast(mut self, capacity: usize) -> Self {
        self.broadcast_capacity = Some(capacity);
        self
    }

    pub fn reporter(mut self, r: impl Reporter + 'static) -> Self {
        self.reporters.push(Arc::new(r));
        self
    }

    pub fn event_filter(mut self, f: impl Fn(&AgentEvent) -> bool + Send + Sync + 'static) -> Self {
        self.event_filter = Some(Box::new(f));
        self
    }

    pub fn cancel_token(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }

    pub fn auto_compact(mut self, enabled: bool) -> Self {
        self.auto_compact = enabled;
        self
    }

    pub fn compact_threshold(mut self, threshold: f64) -> Self {
        self.compact_threshold = threshold;
        self
    }

    /// Set the cumulative character budget retained for tool results in context.
    /// Defaults to 300_000 chars.
    pub fn tool_result_budget(mut self, chars: usize) -> Self {
        self.tool_result_budget = chars;
        self
    }

    /// Pre-populate conversation history (for provider switching mid-session).
    pub fn with_messages(mut self, msgs: Vec<Message>) -> Self {
        self.initial_messages = Some(msgs);
        self
    }

    pub fn tool_extensions(mut self, extensions: Extensions) -> Self {
        self.tool_extensions = extensions;
        self
    }

    pub fn build(self) -> cersei_types::Result<Agent> {
        let provider = self
            .provider
            .ok_or_else(|| CerseiError::Config("Provider is required".into()))?;
        let duplicate_tools = duplicate_tool_names(&self.tools);
        if !duplicate_tools.is_empty() {
            return Err(CerseiError::Config(format!(
                "Tool names must be unique. Duplicate tools: {}",
                duplicate_tools.join(", ")
            )));
        }

        let working_dir = self
            .working_dir
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let broadcast_tx = self.broadcast_capacity.map(|cap| {
            let (tx, _) = broadcast::channel(cap);
            tx
        });

        Ok(Agent {
            provider,
            tools: self.tools,
            system_prompt: self.system_prompt,
            append_system_prompt: self.append_system_prompt,
            model: self.model,
            max_turns: self.max_turns,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            thinking_budget: self.thinking_budget,
            working_dir,
            permission_policy: self.permission_policy.unwrap_or_else(|| Arc::new(AllowAll)),
            network_policy: self.network_policy,
            memory: self.memory,
            session_id: self.session_id,
            hooks: self.hooks,
            mcp_manager: None, // TODO: connect MCP servers
            event_handler: self.event_handler,
            broadcast_tx,
            reporters: self.reporters,
            event_filter: self.event_filter,
            cost_tracker: Arc::new(CostTracker::new()),
            auto_compact: self.auto_compact,
            compact_threshold: self.compact_threshold,
            tool_result_budget: self.tool_result_budget,
            messages: Arc::new(parking_lot::Mutex::new(
                self.initial_messages.unwrap_or_default(),
            )),
            cumulative_usage: Arc::new(parking_lot::Mutex::new(Usage::default())),
            cancel_token: self
                .cancel_token
                .unwrap_or_else(tokio_util::sync::CancellationToken::new),
            tool_extensions: self.tool_extensions,
        })
    }

    /// Build + run in one shot.
    pub async fn run_with(self, prompt: &str) -> cersei_types::Result<AgentOutput> {
        self.build()?.run(prompt).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cersei_provider::{CompletionRequest, CompletionStream, ProviderCapabilities};
    use serde_json::json;

    struct TestProvider;

    #[async_trait::async_trait]
    impl Provider for TestProvider {
        fn name(&self) -> &str {
            "test"
        }

        fn context_window(&self, _model: &str) -> u64 {
            4096
        }

        fn capabilities(&self, _model: &str) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> cersei_types::Result<CompletionStream> {
            unimplemented!("provider is not used in builder tests")
        }
    }

    struct AlphaTool;
    struct AlphaToolDuplicate;
    struct BetaTool;

    #[async_trait::async_trait]
    impl Tool for AlphaTool {
        fn name(&self) -> &str {
            "Alpha"
        }

        fn description(&self) -> &str {
            "Alpha tool"
        }

        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {}})
        }

        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &cersei_tools::ToolContext,
        ) -> cersei_tools::ToolResult {
            unimplemented!("tool is not used in builder tests")
        }
    }

    #[async_trait::async_trait]
    impl Tool for AlphaToolDuplicate {
        fn name(&self) -> &str {
            "Alpha"
        }

        fn description(&self) -> &str {
            "Duplicate alpha tool"
        }

        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {}})
        }

        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &cersei_tools::ToolContext,
        ) -> cersei_tools::ToolResult {
            unimplemented!("tool is not used in builder tests")
        }
    }

    #[async_trait::async_trait]
    impl Tool for BetaTool {
        fn name(&self) -> &str {
            "Beta"
        }

        fn description(&self) -> &str {
            "Beta tool"
        }

        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {}})
        }

        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &cersei_tools::ToolContext,
        ) -> cersei_tools::ToolResult {
            unimplemented!("tool is not used in builder tests")
        }
    }

    #[test]
    fn agent_builder_uses_larger_tool_result_budget_by_default() {
        let builder = AgentBuilder::default();
        assert_eq!(builder.tool_result_budget, DEFAULT_TOOL_RESULT_BUDGET_CHARS);
    }

    #[test]
    fn agent_builder_rejects_duplicate_tool_names() {
        let err = match Agent::builder()
            .provider(TestProvider)
            .tool(AlphaTool)
            .tool(AlphaToolDuplicate)
            .build()
        {
            Ok(_) => panic!("builder should reject duplicate tool names"),
            Err(err) => err,
        };

        match err {
            CerseiError::Config(message) => {
                assert!(message.contains("Tool names must be unique"));
                assert!(message.contains("Alpha"));
            }
            other => panic!("expected config error, got {other:?}"),
        }
    }

    #[test]
    fn agent_builder_accepts_unique_tool_names() {
        let result = Agent::builder()
            .provider(TestProvider)
            .tool(AlphaTool)
            .tool(BetaTool)
            .build();

        assert!(result.is_ok());
    }
}
