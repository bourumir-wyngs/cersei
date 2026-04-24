//! Reusable CLI entrypoint for the Cersei coding agent.

mod agent_filter;
mod app;
mod commands;
mod config;
mod init;
mod input;
mod login;
pub mod memory_tools;
mod network_policy;
mod permissions;
mod prompt;
mod render;
mod repl;
mod reviewer;
mod sessions;
mod signals;
mod status;
mod theme;
mod tools_config;

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "cersei",
    about = "A high-performance AI coding agent",
    version,
    after_help = "Examples:\n  cersei                        Start interactive REPL\n  cersei \"fix the tests\"        Single-shot mode\n  cersei --resume               Resume last session\n  cersei --model gpt-5.4 --max  Use GPT-5.4 with max thinking"
)]
pub struct Cli {
    /// Prompt to run in single-shot mode (omit for REPL)
    #[arg(value_name = "PROMPT")]
    pub prompt: Option<String>,

    /// Run a single prompt and exit
    #[arg(
        short = 'c',
        long = "command",
        value_name = "PROMPT",
        conflicts_with = "prompt"
    )]
    pub command_prompt: Option<String>,

    /// Resume a previous session
    #[arg(long, value_name = "SESSION_ID", num_args = 0..=1, default_missing_value = "last")]
    pub resume: Option<String>,

    /// Model to use (e.g., gpt-4o, gpt-4.1, opus, sonnet)
    #[arg(short, long)]
    pub model: Option<String>,

    /// Provider to use (openai, anthropic)
    #[arg(short, long)]
    pub provider: Option<String>,

    /// Fast mode (low effort, minimal thinking)
    #[arg(long, conflicts_with = "max")]
    pub fast: bool,

    /// Max mode (maximum thinking budget)
    #[arg(long, conflicts_with = "fast")]
    pub max: bool,

    /// Fallback models (comma-separated) for provider switching on error
    #[arg(long, value_delimiter = ',', value_name = "MODELS")]
    pub fallback: Vec<String>,

    /// Auto-approve all tool permissions (CI/headless mode)
    #[arg(long)]
    pub no_permissions: bool,

    /// Output events as NDJSON (for piping)
    #[arg(long)]
    pub json: bool,

    /// Enable verbose/debug logging
    #[arg(short, long)]
    pub verbose: bool,

    /// Working directory override
    #[arg(short = 'C', long)]
    pub directory: Option<String>,

    /// Project name override for permission persistence
    #[arg(long, value_name = "NAME")]
    pub project: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Manage sessions
    Sessions {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Manage configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Manage memory
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
    /// Manage MCP servers
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
    /// Initialize project (.abstract/ directory)
    Init,
    /// Authenticate with a provider
    Login {
        /// Provider: claude, openai, key, status (default: interactive)
        provider: Option<String>,
    },
    /// Remove saved credentials
    Logout,
}

#[derive(Subcommand)]
pub enum SessionAction {
    /// List all sessions
    #[command(alias = "ls")]
    List,
    /// Show a session transcript
    Show { id: String },
    /// Delete a session
    Rm { id: String },
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// Show current configuration
    Show,
    /// Set a configuration value
    Set { key: String, value: String },
}

#[derive(Subcommand)]
pub enum MemoryAction {
    /// Show memory status
    Show,
    /// Clear all memory
    Clear,
}

#[derive(Subcommand)]
pub enum McpAction {
    /// Add an MCP server
    Add {
        name: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// List configured MCP servers
    List,
    /// Remove an MCP server
    Remove { name: String },
}

pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let startup_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project_dir = resolve_project_dir(&startup_dir, cli.directory.as_deref());
    config::initialize_permissions_project_name(&project_dir, cli.project.as_deref());

    if cli.verbose {
        tracing_subscriber::fmt()
            .with_env_filter("abstract=debug,cersei=debug")
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter("abstract=warn,cersei=warn")
            .init();
    }

    let mut config = config::load_for_dir(&project_dir);
    apply_cli_overrides(&cli, &mut config, &project_dir);
    config::ensure_project_config_exists(&project_dir)?;

    match &cli.command {
        Some(Commands::Init) => init::run()?,
        Some(Commands::Login { provider }) => {
            login::run_login(provider.as_deref()).await?;
            return Ok(());
        }
        Some(Commands::Logout) => {
            login::run_logout()?;
            return Ok(());
        }
        Some(Commands::Sessions { action }) => match action {
            SessionAction::List => sessions::list(&config)?,
            SessionAction::Show { id } => sessions::show(&config, id)?,
            SessionAction::Rm { id } => sessions::delete(&config, id)?,
        },
        Some(Commands::Config { action }) => match action {
            ConfigAction::Show => {
                println!("{}", serde_saphyr::to_string(&config)?);
            }
            ConfigAction::Set { key, value } => {
                config_set(&mut config, key, value)?;
                config::save_to(&config, &config::project_config_path())?;
                if key == "effort" {
                    println!(
                        "Set effort = {} (max_tokens = {})",
                        config.effort, config.max_tokens
                    );
                } else {
                    println!("Set {} = {}", key, value);
                }
            }
        },
        Some(Commands::Memory { action }) => match action {
            MemoryAction::Show => sessions::show_memory(&config)?,
            MemoryAction::Clear => sessions::clear_memory(&config)?,
        },
        Some(Commands::Mcp { action }) => match action {
            McpAction::Add { name, command } => {
                if command.is_empty() {
                    anyhow::bail!("MCP server command is required");
                }
                config.mcp_servers.push(config::McpServerEntry {
                    name: name.clone(),
                    command: command[0].clone(),
                    args: command[1..].to_vec(),
                    env: Default::default(),
                });
                config::save_to(&config, &config::project_config_path())?;
                println!("Added MCP server: {}", name);
            }
            McpAction::List => {
                if config.mcp_servers.is_empty() {
                    println!("No MCP servers configured.");
                } else {
                    for s in &config.mcp_servers {
                        println!("  {} — {} {}", s.name, s.command, s.args.join(" "));
                    }
                }
            }
            McpAction::Remove { name } => {
                let before = config.mcp_servers.len();
                config.mcp_servers.retain(|s| s.name != *name);
                if config.mcp_servers.len() < before {
                    config::save_to(&config, &config::project_config_path())?;
                    println!("Removed MCP server: {}", name);
                } else {
                    println!("MCP server '{}' not found.", name);
                }
            }
        },
        None => {
            app::run(cli, config).await?;
        }
    }

    Ok(())
}

fn resolve_project_dir(startup_dir: &Path, cli_directory: Option<&str>) -> PathBuf {
    match cli_directory {
        Some(dir) => {
            let path = PathBuf::from(dir);
            if path.is_absolute() {
                path
            } else {
                startup_dir.join(path)
            }
        }
        None => startup_dir.to_path_buf(),
    }
}

fn apply_cli_overrides(cli: &Cli, config: &mut config::AppConfig, project_dir: &Path) {
    config.working_dir = project_dir.to_path_buf();
    if let Some(m) = &cli.model {
        config.model = resolve_model_alias(m);
    }
    if let Some(p) = &cli.provider {
        config.provider = p.clone();
    }
    if cli.fast {
        config::set_effort_budget(config, config::LOW_EFFORT_BUDGET);
    }
    if cli.max {
        config::set_effort_budget(config, config::MAX_EFFORT_BUDGET);
    }
    if !cli.fallback.is_empty() {
        config.fallback_models = cli.fallback.clone();
    }
    if cli.no_permissions {
        config.permissions_mode = "allow_all".into();
    }
}

fn resolve_model_alias(model: &str) -> String {
    match model {
        "opus" => "anthropic/claude-opus-4-6".into(),
        "sonnet" => "anthropic/claude-sonnet-4-6".into(),
        "haiku" => "anthropic/claude-3-5-haiku-latest".into(),
        "gemini" => "google/gemini-3-flash-preview".into(),
        other if other.contains('/') => other.into(),
        other => other.into(),
    }
}

fn config_set(config: &mut config::AppConfig, key: &str, value: &str) -> anyhow::Result<()> {
    match key {
        "model" => config.model = value.into(),
        "reviewer_model" => config.reviewer_model = value.into(),
        "model_tools" => {
            let included = value
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            config::set_model_tools_from_include_list(config, included);
        }
        "reviewer_tools" => {
            let included = value
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            config::set_reviewer_tools_from_include_list(config, included);
        }
        "exclude_tools" => {
            let excluded = value
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            config::set_exclude_tools(config, excluded);
        }
        "exclude_reviewer_tools" => {
            let excluded = value
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            config::set_exclude_reviewer_tools(config, excluded);
        }
        "provider" => config.provider = value.into(),
        "effort" => {
            let effort = config::parse_effort_budget(value).ok_or_else(|| {
                anyhow::anyhow!("Invalid effort '{value}'. Use a number or low/medium/high/max.")
            })?;
            config::set_effort_budget(config, effort);
        }
        "theme" => config.theme = value.into(),
        "max_turns" => config.max_turns = value.parse()?,
        "max_tokens" => anyhow::bail!("max_tokens is derived automatically as effort * 4"),
        "auto_compact" => config.auto_compact = value.parse()?,
        "graph_memory" => config.graph_memory = value.parse()?,
        "permissions_mode" => config.permissions_mode = value.into(),
        _ => anyhow::bail!("Unknown config key: {key}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_project_dir_uses_startup_dir_when_override_is_missing() {
        let startup_dir = Path::new("/tmp/start");
        assert_eq!(
            resolve_project_dir(startup_dir, None),
            PathBuf::from("/tmp/start")
        );
    }

    #[test]
    fn resolve_project_dir_resolves_relative_override_from_startup_dir() {
        let startup_dir = Path::new("/tmp/start");
        assert_eq!(
            resolve_project_dir(startup_dir, Some("nested/project")),
            PathBuf::from("/tmp/start/nested/project")
        );
    }

    #[test]
    fn resolve_project_dir_preserves_absolute_override() {
        let startup_dir = Path::new("/tmp/start");
        assert_eq!(
            resolve_project_dir(startup_dir, Some("/srv/repo")),
            PathBuf::from("/srv/repo")
        );
    }
}
