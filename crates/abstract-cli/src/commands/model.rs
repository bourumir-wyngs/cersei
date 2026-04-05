use super::CommandAction;
use crate::config::AppConfig;
use cersei_provider::registry::{self, ApiFormat, ProviderEntry};
use serde::Deserialize;

pub async fn run(args: &str, config: &AppConfig) -> anyhow::Result<CommandAction> {
    if args.is_empty() {
        let provider = current_provider(config);
        eprintln!("Current provider: {}", provider.id);
        eprintln!("Current model: {}", config.model);
        eprintln!("\x1b[90mUsage: /model <name>\x1b[0m");
        eprintln!("\x1b[90mExamples: /model gpt-4o, /model anthropic/claude-sonnet-4-6\x1b[0m");
        eprintln!("\x1b[90mAliases: opus, sonnet, haiku, gpt-4o\x1b[0m");
        eprintln!();

        match fetch_models(provider).await {
            Ok(models) if !models.is_empty() => {
                eprintln!("\x1b[36;1mAvailable models ({})\x1b[0m", provider.name);
                for model in &models {
                    let marker = if model == &config.model || config.model.ends_with(&format!("/{model}")) {
                        "*"
                    } else {
                        " "
                    };
                    eprintln!("  {marker} {model}");
                }
            }
            Ok(_) => {
                eprintln!("\x1b[33mNo models returned by {}.\x1b[0m", provider.name);
            }
            Err(err) => {
                eprintln!("\x1b[33mCould not query {} models: {err}\x1b[0m", provider.name);
                if !provider.models.is_empty() {
                    eprintln!("\x1b[36;1mKnown models ({})\x1b[0m", provider.name);
                    for model in provider.models {
                        let marker = if model.id == config.model || config.model.ends_with(&format!("/{}", model.id)) {
                            "*"
                        } else {
                            " "
                        };
                        eprintln!("  {marker} {}", model.id);
                    }
                }
            }
        }

        return Ok(CommandAction::None);
    }

    let resolved = match args.trim() {
        "opus" => "anthropic/claude-opus-4-1".to_string(),
        "sonnet" => "anthropic/claude-sonnet-4-6".to_string(),
        "haiku" => "anthropic/claude-3-5-haiku-latest".to_string(),
        other => other.to_string(),
    };

    let (provider_id, model_id) = resolved
        .split_once('/')
        .map(|(provider, model)| (provider, model))
        .unwrap_or((current_provider(config).id, resolved.as_str()));

    if !is_chat_completions_compatible(provider_id, model_id) {
        anyhow::bail!(
            "Model '{model_id}' is not suitable for the chat/completions agent path. Choose a chat model such as gpt-4o, gpt-4.1, o1, or o3."
        );
    }

    eprintln!("\x1b[90mModel set to: {resolved}\x1b[0m");
    Ok(CommandAction::SwitchAgent {
        provider: resolved.split_once('/').map(|(provider, _)| provider.to_string()),
        model: resolved,
    })
}

fn current_provider(config: &AppConfig) -> &'static ProviderEntry {
    if config.provider != "auto" {
        if let Some(entry) = registry::lookup(&config.provider) {
            return entry;
        }
    }

    if let Some((provider, _)) = config.model.split_once('/') {
        if let Some(entry) = registry::lookup(provider) {
            return entry;
        }
    }

    cersei_provider::router::available_providers()
        .into_iter()
        .next()
        .or_else(|| registry::lookup("openai"))
        .expect("provider registry must contain openai")
}

async fn fetch_models(provider: &ProviderEntry) -> anyhow::Result<Vec<String>> {
    match provider.api_format {
        ApiFormat::OpenAiCompatible => fetch_openai_compatible_models(provider).await,
        ApiFormat::Anthropic => anyhow::bail!("provider does not expose a supported models listing endpoint"),
    }
}

async fn fetch_openai_compatible_models(provider: &ProviderEntry) -> anyhow::Result<Vec<String>> {
    #[derive(Deserialize)]
    struct ModelsResponse {
        data: Vec<ModelInfo>,
    }

    #[derive(Deserialize)]
    struct ModelInfo {
        id: String,
    }

    let key = provider
        .api_key_from_env()
        .ok_or_else(|| anyhow::anyhow!("missing {}", provider.env_keys.join(" or ")))?;

    let url = format!("{}/models", provider.api_base.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .get(url)
        .header("authorization", format!("Bearer {key}"))
        .send()
        .await?;

    if !response.status().is_success() {
        anyhow::bail!("HTTP {}", response.status());
    }

    let mut models: Vec<String> = response
        .json::<ModelsResponse>()
        .await?
        .data
        .into_iter()
        .map(|m| m.id)
        .filter(|model| is_chat_completions_compatible(provider.id, model))
        .collect();

    models.sort();
    models.dedup();
    Ok(models)
}

fn is_chat_completions_compatible(provider_id: &str, model: &str) -> bool {
    if provider_id != "openai" {
        return true;
    }

    let legacy_or_non_chat_prefixes = [
        "babbage-",
        "davinci-",
        "text-embedding-",
        "whisper-",
        "tts-",
        "dall-e-",
        "omni-moderation-",
        "sora-",
    ];
    let non_chat_substrings = [
        "embedding",
        "image",
        "moderation",
        "transcribe",
        "tts",
        "realtime",
        "search-api",
        "computer-use",
        "instruct",
    ];

    if legacy_or_non_chat_prefixes.iter().any(|prefix| model.starts_with(prefix)) {
        return false;
    }
    if non_chat_substrings.iter().any(|needle| model.contains(needle)) {
        return false;
    }
    model.starts_with("gpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
        || model == "chatgpt-4o-latest"
}
