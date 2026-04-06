use super::CommandAction;
use crate::agent_filter::filter_agent_names;
use crate::config::AppConfig;
use cersei_provider::registry::{self, ApiFormat, ProviderEntry};
use serde::Deserialize;

pub async fn run(args: &str, config: &AppConfig) -> anyhow::Result<CommandAction> {
    if args.is_empty() {
        eprintln!("Current model: {}", config.model);
        eprintln!("\x1b[90mUsage: /model <name>\x1b[0m");
        eprintln!(
            "\x1b[90mExamples: /model gpt-5.4, /model gemini-3.1-pro-preview, /model google/gemini-3-flash-preview\x1b[0m"
        );
        eprintln!("\x1b[90mAliases: opus, sonnet, haiku, gemini\x1b[0m");
        eprintln!();

        for provider in display_providers() {
            render_provider_models(provider, &config.model).await;
            eprintln!();
        }

        return Ok(CommandAction::None);
    }

    let resolved = match args.trim() {
        "opus" => "anthropic/claude-opus-4-6".to_string(),
        "sonnet" => "anthropic/claude-sonnet-4-6".to_string(),
        "haiku" => "anthropic/claude-3-5-haiku-latest".to_string(),
        "gemini" => "google/gemini-3-flash-preview".to_string(),
        other => other.to_string(),
    };

    let provider_id = selected_provider_id(config, &resolved);
    let model_id = resolved
        .split_once('/')
        .map(|(_, model)| model)
        .unwrap_or(resolved.as_str());
    let normalized_model = cersei_provider::router::normalize_model_name(provider_id, model_id);
    let normalized = if resolved.contains('/') {
        format!("{provider_id}/{normalized_model}")
    } else {
        normalized_model.clone()
    };

    if !is_chat_completions_compatible(provider_id, &normalized_model) {
        anyhow::bail!(
            "Model '{model_id}' is not suitable for the chat/completions agent path. Choose a chat model such as gpt-4o, gpt-4.1, o1, or o3."
        );
    }

    validate_model_selection(provider_id, &normalized_model).await?;

    eprintln!("\x1b[90mModel set to: {normalized}\x1b[0m");
    Ok(CommandAction::SwitchAgent { model: normalized })
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

fn display_providers() -> Vec<&'static ProviderEntry> {
    ["anthropic", "openai", "google"]
        .into_iter()
        .filter_map(registry::lookup)
        .collect()
}

fn selected_provider_id<'a>(config: &'a AppConfig, model: &'a str) -> &'a str {
    if let Some((provider, _)) = model.split_once('/') {
        return provider;
    }

    match model {
        m if m.starts_with("claude-") => "anthropic",
        m if m.starts_with("gpt-")
            || m.starts_with("o1")
            || m.starts_with("o3")
            || m.starts_with("o4") =>
        {
            "openai"
        }
        m if m.starts_with("gemini-") => "google",
        m if m.starts_with("mistral-") || m.starts_with("codestral-") => "mistral",
        m if m.starts_with("deepseek-") => "deepseek",
        m if m.starts_with("grok-") => "xai",
        _ => current_provider(config).id,
    }
}

async fn render_provider_models(provider: &ProviderEntry, current_model: &str) {
    match fetch_models(provider).await {
        Ok(models) if !models.is_empty() => {
            eprintln!("\x1b[36;1mAvailable models ({})\x1b[0m", provider.name);
            print_models(&models, current_model);
        }
        Ok(_) => {
            eprintln!("\x1b[33mNo models returned by {}.\x1b[0m", provider.name);
        }
        Err(err) => {
            eprintln!(
                "\x1b[33mCould not query {} models: {err}\x1b[0m",
                provider.name
            );
            let fallback_models = fallback_models(provider);
            if !fallback_models.is_empty() {
                eprintln!("\x1b[36;1mKnown models ({})\x1b[0m", provider.name);
                print_models(&fallback_models, current_model);
            }
        }
    }
}

fn fallback_models(provider: &ProviderEntry) -> Vec<String> {
    filter_agent_names(provider.id, provider.models.iter().map(|model| model.id))
}

async fn validate_model_selection(provider_id: &str, model_id: &str) -> anyhow::Result<()> {
    let Some(provider) = registry::lookup(provider_id) else {
        return Ok(());
    };

    if provider.id != "openai" && provider.id != "google" {
        return Ok(());
    }

    let mut available_models = fallback_models(provider);
    if let Ok(models) = fetch_models(provider).await {
        available_models.extend(models);
    }
    available_models.sort();
    available_models.dedup();

    if available_models.is_empty() || available_models.iter().any(|model| model == model_id) {
        return Ok(());
    }

    anyhow::bail!(
        "Unknown {} model '{}'. Use /model to list supported models.",
        provider.name,
        model_id
    )
}

fn normalize_provider_model_id(provider_id: &str, model_id: &str) -> String {
    let model_id = model_id.strip_prefix("models/").unwrap_or(model_id);
    let model_id = model_id
        .strip_prefix(&format!("{provider_id}/"))
        .unwrap_or(model_id);

    cersei_provider::router::normalize_model_name(provider_id, model_id)
}

fn print_models(models: &[String], current_model: &str) {
    for model in models {
        let marker = if model == current_model || current_model.ends_with(&format!("/{model}")) {
            "*"
        } else {
            " "
        };
        eprintln!("  {marker} {model}");
    }
}

async fn fetch_models(provider: &ProviderEntry) -> anyhow::Result<Vec<String>> {
    match provider.api_format {
        ApiFormat::OpenAiCompatible => fetch_openai_compatible_models(provider).await,
        ApiFormat::Anthropic => {
            anyhow::bail!("provider does not expose a supported models listing endpoint")
        }
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
        .map(|m| normalize_provider_model_id(provider.id, &m.id))
        .filter(|model| is_chat_completions_compatible(provider.id, model))
        .collect();

    models = filter_agent_names(provider.id, models);
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

    if legacy_or_non_chat_prefixes
        .iter()
        .any(|prefix| model.starts_with(prefix))
    {
        return false;
    }
    if non_chat_substrings
        .iter()
        .any(|needle| model.contains(needle))
    {
        return false;
    }
    model.starts_with("gpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
        || model.starts_with("gemini-")
        || model == "chatgpt-4o-latest"
}

#[cfg(test)]
mod tests {
    use super::{
        display_providers, fallback_models, is_chat_completions_compatible,
        normalize_provider_model_id, selected_provider_id,
    };
    use crate::config::AppConfig;
    use cersei_provider::registry;

    #[test]
    fn includes_openai_and_google_in_model_listing() {
        let providers = display_providers();
        assert_eq!(providers.len(), 3);
        assert_eq!(providers[0].id, "anthropic");
        assert_eq!(providers[1].id, "openai");
        assert_eq!(providers[2].id, "google");
    }

    #[test]
    fn keeps_google_models_in_fallback_list() {
        let google = registry::lookup("google").unwrap();
        let models = fallback_models(google);
        assert!(models.iter().any(|model| model == "gemini-3-flash-preview"));
        assert!(models.iter().any(|model| model == "gemini-3.1-pro-preview"));
        assert_eq!(models.len(), 2);
    }

    #[test]
    fn allows_gemini_model_switching() {
        assert!(is_chat_completions_compatible(
            "google",
            "gemini-3.1-pro-preview"
        ));
        assert!(is_chat_completions_compatible(
            "google",
            "gemini-pro-latest"
        ));
    }

    #[test]
    fn infers_google_provider_for_bare_gemini_models() {
        let config = AppConfig {
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            ..AppConfig::default()
        };
        assert_eq!(
            selected_provider_id(&config, "gemini-3.1-pro-preview"),
            "google"
        );
    }

    #[test]
    fn normalizes_google_model_ids_from_provider_listing() {
        assert_eq!(
            normalize_provider_model_id("google", "models/gemini-3-flash-preview"),
            "gemini-3-flash-preview"
        );
        assert_eq!(
            normalize_provider_model_id("google", "models/gemini-3.1-pro-preview"),
            "gemini-3.1-pro-preview"
        );
    }
}
