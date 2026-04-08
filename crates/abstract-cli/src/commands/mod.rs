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
mod resume;
mod review;
mod save;

use crate::config::AppConfig;

pub struct CommandRegistry;

pub enum CommandAction {
    None,
    SwitchAgent { model: String },
    ClearHistory,
    Compact,
    LoadSession { messages: Vec<cersei_types::Message>, session_id: String },
    SaveSession { name: String },
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
            "clear" => clear::run().map(|_| CommandAction::ClearHistory),
            "compact" => compact::run(config).map(|_| CommandAction::Compact),
            "cost" => cost::run(session_id).map(|_| CommandAction::None),
            "commit" => commit::run(config).await.map(|_| CommandAction::None),
            "review" => review::run(config).await.map(|_| CommandAction::None),
            "memory" | "mem" => memory::run(config).map(|_| CommandAction::None),
            "model" => model::run(args, config).await,
            "config" | "cfg" => config_cmd::run(args, config).map(|_| CommandAction::None),
            "diff" => diff::run(config).map(|_| CommandAction::None),
            "resume" => resume::run(args, config),
            "save" => Ok(if args.trim().is_empty() {
                eprintln!("\x1b[33mUsage: /save <name>\x1b[0m");
                CommandAction::None
            } else {
                CommandAction::SaveSession { name: args.trim().to_string() }
            }),
            _ => {
                eprintln!("\x1b[33mUnknown command: /{cmd}\x1b[0m");
                eprintln!("Type /help to see available commands.");
                Ok(CommandAction::None)
            }
        };

        result
    }
}
