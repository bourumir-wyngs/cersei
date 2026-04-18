use super::CommandAction;
use crate::agent_filter::filter_agent_names;
use crate::config::AppConfig;
use cersei_provider::registry::{self, ApiFormat, ProviderEntry};
use serde::Deserialize;
use std::collections::HashMap;

// Stores last-displayed number → "provider/model" mapping for quick selection.
// `/model` and `/reviewer` intentionally share the same numeric shortcuts.
static MODEL_INDEX: std::sync::OnceLock<parking_lot::Mutex<HashMap<usize, String>>> =
    std::sync::OnceLock::new();

#[derive(Clone, Copy)]
enum SelectionTarget {
    Coding,
    Reviewer,
}

fn model_index() -> &'static parking_lot::Mutex<HashMap<usize, String>> {
    MODEL_INDEX.get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
}

pub async fn run(args: &str, config: &AppConfig) -> anyhow::Result<CommandAction> {
    run_for(args, config, SelectionTarget::Coding).await
}

pub async fn run_reviewer(args: &str, config: &AppConfig) -> anyhow::Result<CommandAction> {
    run_for(args, config, SelectionTarget::Reviewer).await
}

async fn run_for(
    args: &str,
    config: &AppConfig,
    target: SelectionTarget,
) -> anyhow::Result<CommandAction> {
    let (current_model, index, command_name, current_label, set_message) = match target {
        SelectionTarget::Coding => (
            config.model.as_str(),
            model_index(),
            "/model",
            "Current model",
            "Model set to",
        ),
        SelectionTarget::Reviewer => (
            config.reviewer_model.as_str(),
            model_index(),
            "/reviewer",
            "Current reviewer model",
            "Reviewer model set to",
        ),
    };

    if args.is_empty() {
        eprintln!("{current_label}: {}", current_model);
        eprintln!("\x1b[90mUsage: {command_name} <name|number>\x1b[0m");
        eprintln!(
            "\x1b[90mExamples: {command_name} gpt-5.4, {command_name} gemini-3.1-pro-preview, {command_name} 3\x1b[0m"
        );
        eprintln!("\x1b[90mAliases: opus, sonnet, haiku, gemini\x1b[0m");
        eprintln!();

        let mut new_index: HashMap<usize, String> = HashMap::new();
        let mut counter = 1usize;
        for provider in display_providers() {
            render_provider_models(provider, current_model, &mut counter, &mut new_index).await;
            eprintln!();
        }
        *index.lock() = new_index;

        return Ok(CommandAction::None);
    }

    // Resolve numeric shortcut from the last listing
    let resolved_from_index = if let Ok(n) = args.trim().parse::<usize>() {
        let resolved = index.lock().get(&n).cloned();
        if resolved.is_none() {
            anyhow::bail!(
                "Unknown model shortcut '{}'. Run /model or /reviewer to list models first.",
                n
            );
        }
        resolved
    } else {
        None
    };

    let resolved = if let Some(indexed) = resolved_from_index {
        indexed
    } else {
        match args.trim() {
            "opus" => "anthropic/claude-opus-4-6".to_string(),
            "sonnet" => "anthropic/claude-sonnet-4-6".to_string(),
            "haiku" => "anthropic/claude-3-5-haiku-latest".to_string(),
            "gemini" => "google/gemini-3-flash-preview".to_string(),
            other => other.to_string(),
        }
    };

    let provider_id = selected_provider_id(config, current_model, &resolved);
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

    eprintln!("\x1b[90m{set_message}: {normalized}\x1b[0m");
    Ok(match target {
        SelectionTarget::Coding => CommandAction::SwitchAgent { model: normalized },
        SelectionTarget::Reviewer => CommandAction::SwitchReviewer { model: normalized },
    })
}

fn current_provider<'a>(config: &'a AppConfig, current_model: &'a str) -> &'static ProviderEntry {
    if config.provider != "auto" {
        if let Some(entry) = registry::lookup(&config.provider) {
            return entry;
        }
    }

    if let Some((provider, _)) = current_model.split_once('/') {
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
    ["anthropic", "openai", "google", "xai"]
        .into_iter()
        .filter_map(registry::lookup)
        .collect()
}

fn selected_provider_id<'a>(
    config: &'a AppConfig,
    current_model: &'a str,
    model: &'a str,
) -> &'a str {
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
        _ => current_provider(config, current_model).id,
    }
}

async fn render_provider_models(
    provider: &ProviderEntry,
    current_model: &str,
    counter: &mut usize,
    index: &mut HashMap<usize, String>,
) {
    match fetch_models(provider).await {
        Ok(models) if !models.is_empty() => {
            eprintln!("\x1b[36;1mAvailable models ({})\x1b[0m", provider.name);
            print_models(provider.id, &models, current_model, counter, index);
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
                print_models(provider.id, &fallback_models, current_model, counter, index);
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
        "Unknown {} model '{}'. Use /model or /reviewer to list supported models.",
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

fn print_models(
    provider_id: &str,
    models: &[String],
    current_model: &str,
    counter: &mut usize,
    index: &mut HashMap<usize, String>,
) {
    for model in models {
        let marker = if model == current_model || current_model.ends_with(&format!("/{model}")) {
            "*"
        } else {
            " "
        };
        let n = *counter;
        let qualified = format!("{provider_id}/{model}");
        index.insert(n, qualified);
        eprintln!("  {marker} {:2}. {model}", n);
        *counter += 1;
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
    if provider_id == "xai" {
        return !model.starts_with("grok-imagine-");
    }

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
        model_index, normalize_provider_model_id, run_reviewer, selected_provider_id, CommandAction,
    };
    use crate::config::AppConfig;
    use cersei_provider::registry;

    #[test]
    fn includes_xai_in_model_listing() {
        let providers = display_providers();
        assert_eq!(providers.len(), 4);
        assert_eq!(providers[0].id, "anthropic");
        assert_eq!(providers[1].id, "openai");
        assert_eq!(providers[2].id, "google");
        assert_eq!(providers[3].id, "xai");
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
    fn keeps_xai_models_in_fallback_list() {
        let xai = registry::lookup("xai").unwrap();
        let models = fallback_models(xai);
        assert_eq!(
            models,
            vec!["grok-4.20-0309-non-reasoning", "grok-4.20-0309-reasoning"]
        );
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
            selected_provider_id(&config, &config.model, "gemini-3.1-pro-preview"),
            "google"
        );
    }

    #[test]
    fn filters_xai_image_generation_models_from_chat_path() {
        assert!(is_chat_completions_compatible(
            "xai",
            "grok-4.20-0309-reasoning"
        ));
        assert!(!is_chat_completions_compatible("xai", "grok-imagine-image"));
        assert!(!is_chat_completions_compatible("xai", "grok-imagine-video"));
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

    #[tokio::test]
    async fn reviewer_numeric_shortcuts_share_model_listing_index() {
        model_index()
            .lock()
            .insert(4242, "anthropic/claude-sonnet-4-6".to_string());

        let config = AppConfig::default();
        let action = run_reviewer("4242", &config).await.unwrap();

        match action {
            CommandAction::SwitchReviewer { model } => {
                assert_eq!(model, "anthropic/claude-sonnet-4-6");
            }
            _ => panic!("unexpected action"),
        }
    }

    #[tokio::test]
    async fn missing_numeric_shortcut_returns_explicit_error() {
        let config = AppConfig::default();
        let err = match run_reviewer("99999", &config).await {
            Ok(_) => panic!("expected missing shortcut to fail"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains("Unknown model shortcut '99999'"));
    }
}
