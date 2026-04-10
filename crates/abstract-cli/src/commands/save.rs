use super::CommandAction;
use crate::config::AppConfig;

pub fn run(
    args: &str,
    config: &AppConfig,
    messages: &[cersei_types::Message],
) -> anyhow::Result<CommandAction> {
    let name = args.trim();
    if name.is_empty() {
        anyhow::bail!("Usage: /save <name>");
    }

    crate::sessions::save_named(config, name, messages)?;
    eprintln!("\x1b[90m  Session saved as '{}'\x1b[0m", name);
    Ok(CommandAction::None)
}
