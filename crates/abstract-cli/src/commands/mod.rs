//! Slash command registry and dispatch.

mod clear;
mod commit;
mod compact;
mod config_cmd;
mod cost;
mod diff;
mod help;
mod memory;
mod model;
mod provider;
mod resume;
mod review;

use crate::config::AppConfig;

pub struct CommandRegistry;

pub enum CommandAction {
    None,
    SwitchAgent {
        model: String,
        provider: Option<String>,
    },
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self
    }

    pub async fn execute(
        &mut self,
        cmd: &str,
        args: &str,
        config: &AppConfig,
        session_id: &str,
    ) -> anyhow::Result<CommandAction> {
        let result = match cmd {
            "help" | "h" | "?" => help::run().map(|_| CommandAction::None),
            "clear" => clear::run().map(|_| CommandAction::None),
            "compact" => compact::run(config).map(|_| CommandAction::None),
            "cost" => cost::run(session_id).map(|_| CommandAction::None),
            "commit" => commit::run(config).await.map(|_| CommandAction::None),
            "review" => review::run(config).await.map(|_| CommandAction::None),
            "memory" | "mem" => memory::run(config).map(|_| CommandAction::None),
            "model" => model::run(args, config).await,
            "provider" => provider::run(args, config),
            "config" | "cfg" => config_cmd::run(args, config).map(|_| CommandAction::None),
            "diff" => diff::run(config).map(|_| CommandAction::None),
            "resume" => resume::run(args, config).map(|_| CommandAction::None),
            _ => {
                eprintln!("\x1b[33mUnknown command: /{cmd}\x1b[0m");
                eprintln!("Type /help to see available commands.");
                Ok(CommandAction::None)
            }
        };

        result
    }
}
