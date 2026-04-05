use super::CommandAction;
use crate::config::AppConfig;

pub fn run(args: &str, config: &AppConfig) -> anyhow::Result<CommandAction> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        let mut available: Vec<_> = cersei_provider::router::available_providers()
            .into_iter()
            .filter(|entry| entry.requires_key() || entry.id == config.provider)
            .collect();
        available.sort_by_key(|entry| entry.id);
        eprintln!("Current provider: {}", config.provider);
        eprintln!("\x1b[90mUsage: /provider <name> [model]\x1b[0m");
        eprintln!("\x1b[90mExamples: /provider openai, /provider anthropic claude-sonnet-4-6\x1b[0m");
        eprintln!();
        eprintln!("\x1b[36;1mAvailable providers\x1b[0m");
        for entry in available {
            let marker = if entry.id == config.provider { "*" } else { " " };
            eprintln!("  {marker} {} ({})", entry.id, entry.default_model);
        }
        return Ok(CommandAction::None);
    }

    if trimmed == "list" {
        eprintln!("Current provider: {}", config.provider);
        eprintln!();
        eprintln!("\x1b[36;1mKnown providers\x1b[0m");
        for entry in cersei_provider::router::all_providers() {
            let marker = if entry.id == config.provider { "*" } else { " " };
            let status = if entry.api_key_from_env().is_some() || !entry.requires_key() {
                "configured"
            } else {
                "unconfigured"
            };
            eprintln!("  {marker} {} ({}, {})", entry.id, entry.default_model, status);
        }
        return Ok(CommandAction::None);
    }

    let mut parts = trimmed.split_whitespace();
    let provider = parts.next().unwrap_or_default();
    let explicit_model = parts.next();

    let model = if let Some(model) = explicit_model {
        format!("{provider}/{model}")
    } else {
        let entry = cersei_provider::router::all_providers()
            .iter()
            .find(|entry| entry.id == provider)
            .ok_or_else(|| anyhow::anyhow!("Unknown provider: {provider}"))?;
        format!("{provider}/{}", entry.default_model)
    };

    eprintln!("\x1b[90mProvider set to: {provider}\x1b[0m");
    eprintln!("\x1b[90mModel set to: {model}\x1b[0m");
    Ok(CommandAction::SwitchAgent {
        model,
        provider: Some(provider.to_string()),
    })
}
