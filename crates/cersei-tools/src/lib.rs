//! cersei-tools: Tool trait, built-in tool implementations, and permission system.

pub mod ask_user;
pub mod bash;
pub mod bash_classifier;
pub mod browser_tool;
pub mod cargo_tool;
#[cfg(feature = "cas")]
pub mod cas;
pub mod config_tool;
pub mod cron;
pub mod file_history;
pub mod file_history_tool;
pub mod file_tool;
pub mod file_xedit;
pub mod file_xgrep;
pub mod file_xmultiread;
pub mod file_xread;
pub mod file_xrevert;
pub mod file_xwrite;
pub mod git_tool;
pub mod git_utils;
pub mod glob_tool;
pub mod grep_tool;
pub mod list_directory;
pub mod mysql_tool;
pub mod network_policy;
pub mod spreadsheet_tool;
pub mod pdf_tool;
pub mod notebook_edit;
pub mod npm_tool;
pub mod npx_tool;
pub mod permissions;
pub mod plan_mode;
pub mod postgres_tool;
pub mod powershell;
pub mod process_tool;
pub mod pytest_tool;
pub mod remote_trigger;
pub mod review_tool;
pub mod send_message;
mod shell_sandbox;
pub mod skill_tool;
pub mod skills;
pub mod sleep;
pub mod structure_tool;
pub mod synthetic_output;
pub mod tasks;
pub mod todo_write;
pub mod tool_search;
pub mod wasm_tests_tool;
pub mod web_fetch;
pub mod web_search;
pub mod web_tests_tool;
pub mod worktree;
pub mod xfile_storage;
pub mod xfile_sync;

use async_trait::async_trait;
use cersei_mcp::McpManager;
use cersei_types::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

// ─── Tool trait ──────────────────────────────────────────────────────────────

#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name (used by the model to invoke it).
    fn name(&self) -> &str;

    /// Human-readable description shown to the model.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's input parameters.
    fn input_schema(&self) -> Value;

    /// Permission level required for this tool.
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    /// Category for grouping in tool listings.
    fn category(&self) -> ToolCategory {
        ToolCategory::Custom
    }

    /// Validate or short-circuit tool execution before permissions are checked.
    ///
    /// This is useful for rejecting obviously misrouted tool calls such as
    /// `Bash` invocations that should use a dedicated tool instead.
    fn preflight(&self, _input: &Value, _ctx: &ToolContext) -> Option<ToolResult> {
        None
    }

    /// Execute the tool with the given JSON input.
    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult;

    /// Convert to a ToolDefinition for the provider.
    fn to_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
        }
    }
}

/// Typed tool execution trait — used with `#[derive(Tool)]`.
#[async_trait]
pub trait ToolExecute: Send + Sync {
    type Input: serde::de::DeserializeOwned + schemars::JsonSchema;

    async fn run(&self, input: Self::Input, ctx: &ToolContext) -> ToolResult;
}

// ─── Permission levels ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionLevel {
    None,
    ReadOnly,
    Write,
    Execute,
    Dangerous,
    Forbidden,
}

// ─── Tool categories ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    FileSystem,
    Shell,
    Testing,
    Web,
    Memory,
    Orchestration,
    Mcp,
    Custom,
}

// ─── Tool result ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewSource {
    CheckpointDiff,
    GitDiff,
}

impl ReviewSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::CheckpointDiff => "checkpoint diff",
            Self::GitDiff => "git diff",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReviewRequest {
    pub diff: String,
    pub source: ReviewSource,
}

impl ReviewRequest {
    pub fn checkpoint(diff: String) -> Self {
        Self {
            diff,
            source: ReviewSource::CheckpointDiff,
        }
    }

    pub fn git_diff(diff: String) -> Self {
        Self {
            diff,
            source: ReviewSource::GitDiff,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReviewResponse {
    pub review: String,
    pub reviewer_model: String,
    pub reviewer_session_id: String,
}

#[derive(Debug, Clone)]
pub struct XFileStorageScope {
    pub session_id: String,
}

impl XFileStorageScope {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
        }
    }
}

#[async_trait]
pub trait ReviewExecutor: Send + Sync {
    async fn review(&self, request: ReviewRequest) -> std::result::Result<ReviewResponse, String>;
}

#[derive(Clone)]
pub struct ReviewService {
    executor: Arc<dyn ReviewExecutor>,
}

impl ReviewService {
    pub fn new(executor: Arc<dyn ReviewExecutor>) -> Self {
        Self { executor }
    }

    pub async fn review(
        &self,
        request: ReviewRequest,
    ) -> std::result::Result<ReviewResponse, String> {
        self.executor.review(request).await
    }
}

impl ToolResult {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            metadata: None,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            metadata: None,
        }
    }

    pub fn with_metadata(mut self, meta: Value) -> Self {
        self.metadata = Some(meta);
        self
    }
}

// ─── Tool context ────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ToolContext {
    pub working_dir: PathBuf,
    pub session_id: String,
    pub permissions: Arc<dyn permissions::PermissionPolicy>,
    pub cost_tracker: Arc<CostTracker>,
    pub mcp_manager: Option<Arc<McpManager>>,
    pub extensions: Extensions,
    pub network_policy: Option<Arc<dyn network_policy::NetworkPolicy>>,
}

impl Default for ToolContext {
    fn default() -> Self {
        Self {
            working_dir: PathBuf::from("."),
            session_id: "default".into(),
            permissions: Arc::new(permissions::AllowAll),
            cost_tracker: Arc::new(CostTracker::default()),
            mcp_manager: None,
            extensions: Extensions::default(),
            network_policy: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub mysql: Option<MySqlToolConfig>,
    pub postgresql: Option<PostgresToolConfig>,
    pub browser: Option<BrowserToolConfig>,
    pub wasm_tests: Option<WasmTestsToolConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MySqlToolConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: Option<String>,
    pub readonly: bool,
}

impl Default for MySqlToolConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 3306,
            user: "root".into(),
            password: String::new(),
            database: None,
            readonly: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PostgresToolConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: Option<String>,
    pub readonly: bool,
}

impl Default for PostgresToolConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 5432,
            user: "postgres".into(),
            password: String::new(),
            database: None,
            readonly: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserToolConfig {
    pub window: BrowserWindowConfig,
    pub url: Option<String>,
    pub notes: Option<String>,
}

impl Default for BrowserToolConfig {
    fn default() -> Self {
        Self {
            window: BrowserWindowConfig::default(),
            url: None,
            notes: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserWindowConfig {
    pub width: u32,
    pub height: u32,
}

impl Default for BrowserWindowConfig {
    fn default() -> Self {
        Self {
            width: 1440,
            height: 1000,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WasmTestsToolConfig {
    pub network: Option<String>,
}

static GLOBAL_TOOLS_CONFIG: once_cell::sync::Lazy<parking_lot::RwLock<ToolsConfig>> =
    once_cell::sync::Lazy::new(|| parking_lot::RwLock::new(ToolsConfig::default()));

pub fn set_global_tools_config(config: ToolsConfig) {
    *GLOBAL_TOOLS_CONFIG.write() = config;
}

pub fn global_tools_config() -> ToolsConfig {
    GLOBAL_TOOLS_CONFIG.read().clone()
}

/// Type-map for injecting custom data into the tool context.
#[derive(Clone, Default)]
pub struct Extensions {
    data: Arc<dashmap::DashMap<std::any::TypeId, Arc<dyn std::any::Any + Send + Sync>>>,
}

impl Extensions {
    pub fn insert<T: Send + Sync + 'static>(&self, val: T) {
        self.data.insert(std::any::TypeId::of::<T>(), Arc::new(val));
    }

    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.data
            .get(&std::any::TypeId::of::<T>())
            .and_then(|v| Arc::clone(v.value()).downcast::<T>().ok())
    }
}

/// Tracks cumulative token usage and cost.
pub struct CostTracker {
    usage: parking_lot::Mutex<Usage>,
}

impl CostTracker {
    pub fn new() -> Self {
        Self {
            usage: parking_lot::Mutex::new(Usage::default()),
        }
    }

    pub fn add(&self, usage: &Usage) {
        self.usage.lock().merge(usage);
    }

    pub fn current(&self) -> Usage {
        self.usage.lock().clone()
    }
}

impl Default for CostTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Shell state (persisted across Bash invocations) ─────────────────────────

#[derive(Debug, Clone, Default)]
pub struct ShellState {
    pub cwd: Option<PathBuf>,
    pub env_vars: HashMap<String, String>,
}

static SHELL_STATE_REGISTRY: once_cell::sync::Lazy<
    dashmap::DashMap<String, Arc<parking_lot::Mutex<ShellState>>>,
> = once_cell::sync::Lazy::new(dashmap::DashMap::new);

pub fn session_shell_state(session_id: &str) -> Arc<parking_lot::Mutex<ShellState>> {
    SHELL_STATE_REGISTRY
        .entry(session_id.to_string())
        .or_insert_with(|| Arc::new(parking_lot::Mutex::new(ShellState::default())))
        .clone()
}

pub fn clear_session_shell_state(session_id: &str) {
    SHELL_STATE_REGISTRY.remove(session_id);
}

// ─── Built-in tool sets ──────────────────────────────────────────────────────

/// All built-in tools.
pub fn all() -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    tools.extend(default_filesystem());
    tools.extend(shell());
    tools.extend(package_managers());
    tools.extend(testing());
    tools.extend(web());
    tools.extend(data());
    tools.extend(vcs());
    tools.extend(planning());
    tools.extend(scheduling());
    tools.extend(orchestration());
    tools.push(Box::new(ask_user::AskUserQuestionTool));
    // SyntheticOutput is intentionally excluded from the default set — it's for SDK/coordinator
    // sessions only. Add it explicitly via AgentBuilder::tool() when needed.
    tools.push(Box::new(config_tool::ConfigTool));
    #[cfg(feature = "cas")]
    tools.extend(math());
    tools
}

/// All coding-oriented tools (filesystem + shell + web).
pub fn coding() -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    tools.extend(default_filesystem());
    tools.extend(shell());
    tools.extend(package_managers());
    tools.extend(testing());
    tools.extend(web());
    tools.extend(data());
    tools.extend(vcs());
    tools
}

/// Data tools: SQL/database access.
pub fn data() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(mysql_tool::MySqlTool),
        Box::new(postgres_tool::PostgresTool),
    ]
}

/// File system tools: XFileStorage-backed file tools plus notebook/history helpers.
pub fn filesystem() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(file_xread::XReadTool),
        Box::new(file_xmultiread::XMultiReadTool),
        Box::new(file_xwrite::XWriteTool),
        Box::new(file_xedit::XEditTool),
        Box::new(file_tool::FileTool),
        Box::new(file_xrevert::XRevertTool),
        Box::new(glob_tool::GlobTool),
        Box::new(file_xgrep::XGrepTool),
        Box::new(list_directory::ListDirectoryTool),
        Box::new(notebook_edit::NotebookEditTool),
        Box::new(file_history_tool::FileHistoryTool),
        Box::new(review_tool::ReviewTool),
        Box::new(structure_tool::StructureTool),
        Box::new(spreadsheet_tool::SpreadsheetInfoTool),
        Box::new(spreadsheet_tool::SpreadsheetReadTool),
        Box::new(pdf_tool::PdfReadTool),
    ]
}

/// Default filesystem tools used by `coding()` and `all()`.
///
/// This exposes only the XFileStorage-backed file tools and related helpers.
fn default_filesystem() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(file_xread::XReadTool),
        Box::new(file_xmultiread::XMultiReadTool),
        Box::new(file_xwrite::XWriteTool),
        Box::new(file_xedit::XEditTool),
        Box::new(file_tool::FileTool),
        Box::new(file_xrevert::XRevertTool),
        Box::new(glob_tool::GlobTool),
        Box::new(file_xgrep::XGrepTool),
        Box::new(list_directory::ListDirectoryTool),
        Box::new(notebook_edit::NotebookEditTool),
        Box::new(file_history_tool::FileHistoryTool),
        Box::new(review_tool::ReviewTool),
        Box::new(structure_tool::StructureTool),
        Box::new(spreadsheet_tool::SpreadsheetInfoTool),
        Box::new(spreadsheet_tool::SpreadsheetReadTool),
        Box::new(pdf_tool::PdfReadTool),
    ]
}

/// Shell tools: Bash, PowerShell, Process.
pub fn shell() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(bash::BashTool),
        Box::new(powershell::PowerShellTool),
        Box::new(process_tool::ProcessTool),
    ]
}

/// Package manager tools: Npm, Npx, Cargo.
pub fn package_managers() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(npm_tool::NpmTool),
        Box::new(npx_tool::NpxTool),
        Box::new(cargo_tool::CargoTool),
    ]
}

/// Testing tools: Pytest, web_tests.
pub fn testing() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(pytest_tool::PytestTool),
        Box::new(web_tests_tool::WebTestsTool),
        Box::new(wasm_tests_tool::WasmTestsTool),
    ]
}

/// Web tools: WebFetch, WebSearch, and local browser automation.
pub fn web() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(web_fetch::WebFetchTool),
        Box::new(web_search::WebSearchTool),
        Box::new(browser_tool::BrowserWindowTool),
        Box::new(browser_tool::BrowserNavigateTool),
        Box::new(browser_tool::BrowserConsoleTool),
        Box::new(browser_tool::BrowserDomTool),
        Box::new(browser_tool::BrowserClickTool),
        Box::new(browser_tool::BrowserInputTool),
        Box::new(browser_tool::BrowserNetworkTool),
        Box::new(browser_tool::BrowserCssTool),
        Box::new(browser_tool::BrowserStorageTool),
    ]
}

/// Planning tools: EnterPlanMode, ExitPlanMode, TodoWrite.
pub fn planning() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(plan_mode::EnterPlanModeTool),
        Box::new(plan_mode::ExitPlanModeTool),
        Box::new(todo_write::TodoWriteTool),
    ]
}

/// Scheduling tools: Cron (Create/List/Delete), Sleep, RemoteTrigger.
pub fn scheduling() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(cron::CronCreateTool),
        Box::new(cron::CronListTool),
        Box::new(cron::CronDeleteTool),
        Box::new(sleep::SleepTool),
        Box::new(remote_trigger::RemoteTriggerTool),
    ]
}

/// Orchestration tools: SendMessage, Tasks, Worktree.
pub fn orchestration() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(send_message::SendMessageTool),
        Box::new(tasks::TaskCreateTool),
        Box::new(tasks::TaskGetTool),
        Box::new(tasks::TaskUpdateTool),
        Box::new(tasks::TaskListTool),
        Box::new(tasks::TaskStopTool),
        Box::new(tasks::TaskOutputTool),
        Box::new(worktree::EnterWorktreeTool),
        Box::new(worktree::ExitWorktreeTool),
    ]
}

/// Math / CAS tools (requires `cas` feature and system giac library).
#[cfg(feature = "cas")]
pub fn math() -> Vec<Box<dyn Tool>> {
    vec![Box::new(cas::CasTool)]
}

/// Read-only VCS tool: a single `Git` tool with command dispatch.
pub fn vcs() -> Vec<Box<dyn Tool>> {
    vec![Box::new(git_tool::GitTool)]
}

/// No tools (for pure chat agents).
pub fn none() -> Vec<Box<dyn Tool>> {
    vec![]
}
