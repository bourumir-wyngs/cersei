//! Slash command registry and dispatch.

mod changes;
mod checkpoint;
mod clear;
mod commit;
mod compact;
mod config_cmd;
mod cost;
mod diff;
mod effort;
mod help;
mod memory;
mod model;
mod resume;
mod review;
mod rollback;
pub(crate) mod tools;

use crate::config::AppConfig;

pub struct CommandRegistry;

pub enum CommandAction {
    None,
    RunReviewer {
        diff: String,
        hint: String,
    },
    SwitchAgent {
        model: String,
    },
    SwitchReviewer {
        model: String,
    },
    SwitchEffort {
        effort: u32,
    },
    ClearHistory,
    Compact,
    InjectUserMessage {
        message: String,
    },
    LoadSession {
        messages: Vec<cersei_types::Message>,
        session_id: String,
    },
    SaveSession {
        name: String,
    },
    ShowTools,
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
            "changes" => changes::run(args, session_id).map_err(anyhow::Error::msg),
            "clear" => clear::run().map(|_| CommandAction::ClearHistory),
            "checkpoint" => checkpoint::run(args, session_id).map_err(anyhow::Error::msg),
            "compact" => compact::run(config).map(|_| CommandAction::Compact),
            "cost" => cost::run(session_id).map(|_| CommandAction::None),
            "commit" => commit::run(config).await.map(|_| CommandAction::None),
            "review" => review::run(args, config).await,
            "reviewer" => model::run_reviewer(args, config).await,
            "tools" => tools::run(args),
            "effort" => effort::run(args, config),
            "memory" | "mem" => memory::run(config).map(|_| CommandAction::None),
            "model" => model::run(args, config).await,
            "config" | "cfg" => config_cmd::run(args, config).map(|_| CommandAction::None),
            "diff" => diff::run(config).map(|_| CommandAction::None),
            "rollback" => rollback::run(args, session_id)
                .await
                .map_err(anyhow::Error::msg),
            "resume" => resume::run(args, config),
            "delete" | "del" => {
                if args.trim().is_empty() {
                    eprintln!("\x1b[33mUsage: /delete <session-id>\x1b[0m");
                    Ok(CommandAction::None)
                } else {
                    crate::sessions::delete(config, args.trim()).map(|_| CommandAction::None)
                }
            }
            "save" => Ok(if args.trim().is_empty() {
                eprintln!("\x1b[33mUsage: /save <name>\x1b[0m");
                CommandAction::None
            } else {
                CommandAction::SaveSession {
                    name: args.trim().to_string(),
                }
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
