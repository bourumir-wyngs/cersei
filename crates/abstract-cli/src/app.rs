//! Application state, agent construction, and lifecycle management.

use crate::config::AppConfig;
use crate::network_policy::CliNetworkPolicy;
use crate::permissions::CliPermissionPolicy;
use crate::prompt;
use crate::repl;
use crate::sessions;
use crate::theme::Theme;
use crate::tools_config;
use crate::Cli;

use cersei_agent::effort::EffortLevel;
use cersei_mcp::McpServerConfig;
use cersei_memory::manager::MemoryManager;
use cersei_tools::network_policy::sandbox_warning;
use cersei_tools::permissions::AllowAll;
use cersei_tools::Extensions;
use cersei_types::Message;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Run the application (REPL or single-shot).
pub async fn run(cli: Cli, mut config: AppConfig) -> anyhow::Result<()> {
    let theme = Theme::from_name(&config.theme);
    let tool_extensions = tools_config::load_extensions_from_start_dir()?;

    print_startup_warnings();

    // Resolve or create session ID
    let session_id = if let Some(ref resume) = cli.resume {
        if resume == "last" {
            sessions::last_session_id(&config)
                .ok_or_else(|| anyhow::anyhow!("No previous session found"))?
        } else {
            resume.clone()
        }
    } else {
        uuid::Uuid::new_v4().to_string()
    };

    // Build memory manager with graph memory
    let memory_manager = build_memory_manager(&config)?;

    let running = Arc::new(AtomicBool::new(false));

    // Install signal handlers
    let signal_handle = crate::signals::install(running.clone())?;
    let cancel_token = signal_handle.token();

    // Build the initial agent
    let (agent, resolved_model) = build_agent(
        &config.model,
        &config,
        &memory_manager,
        &session_id,
        cancel_token.clone(),
        None,
        tool_extensions.clone(),
    )?;
    config.model = resolved_model;

    // Show startup banner
    let effort = EffortLevel::from_str(&config.effort);
    if !cli.json {
        print_banner(&config, &session_id, &effort);
    }

    // Dispatch to REPL or single-shot
    let one_shot_prompt = cli.command_prompt.as_deref().or(cli.prompt.as_deref());

    if let Some(prompt_text) = one_shot_prompt {
        repl::run_single_shot(
            agent,
            prompt_text,
            &theme,
            &session_id,
            &config,
            &memory_manager,
            &tool_extensions,
            cli.json,
            running,
            signal_handle,
        )
        .await
    } else {
        repl::run_repl(
            agent,
            &theme,
            &session_id,
            &config,
            &memory_manager,
            &tool_extensions,
            cli.json,
            running,
            signal_handle,
        )
        .await
    }
}

/// Build an agent for a given model string. Reusable for initial build and provider switching.
pub fn build_agent(
    model_string: &str,
    config: &AppConfig,
    memory_manager: &MemoryManager,
    session_id: &str,
    cancel_token: CancellationToken,
    existing_messages: Option<Vec<Message>>,
    tool_extensions: Extensions,
) -> anyhow::Result<(cersei::Agent, String)> {
    let (provider, resolved_model) =
        cersei_provider::from_model_string(model_string).map_err(|e| anyhow::anyhow!("{e}"))?;

    let system_prompt = prompt::build_cli_system_prompt(config, memory_manager);
    let effort = EffortLevel::from_str(&config.effort);
    let theme = Theme::from_name(&config.theme);

    let mcp_configs: Vec<McpServerConfig> = config
        .mcp_servers
        .iter()
        .map(|s| {
            let args_ref: Vec<&str> = s.args.iter().map(|a| a.as_str()).collect();
            let mut cfg = McpServerConfig::stdio(&s.name, &s.command, &args_ref);
            cfg.env = s.env.clone();
            cfg
        })
        .collect();

    let mut builder = cersei::Agent::builder()
        .provider(provider)
        .tools(cersei_tools::all())
        .system_prompt(system_prompt)
        .model(&resolved_model)
        .max_turns(config.max_turns)
        .max_tokens(config.max_tokens)
        .auto_compact(config.auto_compact)
        .enable_broadcast(512)
        .cancel_token(cancel_token)
        .session_id(session_id)
        .working_dir(&config.working_dir)
        .tool_extensions(tool_extensions);

    // Permission policy
    if config.permissions_mode == "allow_all" {
        builder = builder.permission_policy(AllowAll);
    } else {
        builder = builder
            .permission_policy(CliPermissionPolicy::new(&theme))
            .network_policy(CliNetworkPolicy::new(&theme));
    }

    // Effort level
    let budget = effort.thinking_budget_tokens();
    builder = builder.thinking_budget(budget);
    if let Some(temp) = effort.temperature() {
        builder = builder.temperature(temp);
    }

    // MCP servers
    for mcp in mcp_configs {
        builder = builder.mcp_server(mcp);
    }

    // Inject existing messages (for provider switching)
    if let Some(msgs) = existing_messages {
        builder = builder.with_messages(msgs);
    }

    let agent = builder.build()?;
    Ok((agent, resolved_model))
}

fn build_memory_manager(config: &AppConfig) -> anyhow::Result<MemoryManager> {
    let mut mm = MemoryManager::new(&config.working_dir);

    #[cfg(feature = "graph")]
    if config.graph_memory {
        let graph_path = crate::config::graph_db_path();
        if let Some(parent) = graph_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        mm = mm
            .with_graph(&graph_path)
            .map_err(|e| anyhow::anyhow!("Failed to open graph memory: {e}"))?;
    }

    Ok(mm)
}

fn print_banner(config: &AppConfig, session_id: &str, effort: &EffortLevel) {
    let short_id = if session_id.len() > 8 {
        &session_id[..8]
    } else {
        session_id
    };

    eprintln!(
        "\x1b[36;1mcersei\x1b[0m \x1b[90mv{} | {} | {:?} effort | session {}\x1b[0m",
        env!("CARGO_PKG_VERSION"),
        config.model,
        effort,
        short_id,
    );
    eprintln!("\x1b[90mType /help for commands, Ctrl+C to cancel, Ctrl+C×2 to exit\x1b[0m\n");
}

fn print_startup_warnings() {
    if let Some(warning) = sandbox_warning() {
        eprintln!("\x1b[33;1mWarning:\x1b[0m {warning}\n");
    }
}
