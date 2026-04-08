use super::CommandAction;
use crate::config::AppConfig;
use crate::sessions;
use cersei_memory::session_storage;

pub fn run(args: &str, config: &AppConfig) -> anyhow::Result<CommandAction> {
    if args.is_empty() {
        sessions::list(config)?;
        eprintln!("\n\x1b[90mUsage: /resume <session-id>\x1b[0m");
        return Ok(CommandAction::None);
    }

    let session_id = args.trim();
    let path = session_storage::transcript_path(&config.working_dir, session_id);

    if !path.exists() {
        anyhow::bail!("Session '{}' not found", session_id);
    }

    let entries = session_storage::load_transcript(&path)
        .map_err(|e| anyhow::anyhow!("Failed to load session: {e}"))?;
    let messages = session_storage::messages_from_transcript(&entries);

    if messages.is_empty() {
        anyhow::bail!("Session '{}' is empty", session_id);
    }

    eprintln!(
        "\x1b[90m  Loaded session {} ({} messages)\x1b[0m",
        &session_id[..8.min(session_id.len())],
        messages.len()
    );

    Ok(CommandAction::LoadSession {
        messages,
        session_id: session_id.to_string(),
    })
}
