use anyhow::{anyhow, Context};
use cersei::prelude::*;
use cersei_provider::from_model_string;

const FLASH_MODEL: &str = "google/gemini-3-flash-preview";
const PRO_MODEL: &str = "google/gemini-3.1-pro-preview";
const BOOTSTRAP_PROMPT: &str = "Reply with exactly READY.";
const PI_PROMPT: &str = "Explain pi in one sentence.";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    require_gemini_key()?;

    println!("Building flash agent with {FLASH_MODEL}");
    let flash_agent = build_agent(FLASH_MODEL, None)?;

    println!("Running bootstrap prompt on {FLASH_MODEL}");
    let flash_output = flash_agent
        .run(BOOTSTRAP_PROMPT)
        .await
        .with_context(|| format!("bootstrap run failed on {FLASH_MODEL}"))?;
    println!("Flash output: {}", flash_output.text().trim());

    let messages = flash_agent.messages();

    println!("Switching to {PRO_MODEL}");
    let pro_agent = build_agent(PRO_MODEL, Some(messages))?;

    println!("Running pi prompt on {PRO_MODEL}");
    let pro_output = pro_agent
        .run(PI_PROMPT)
        .await
        .with_context(|| format!("pi prompt failed on {PRO_MODEL}"))?;
    println!("Pro output: {}", pro_output.text().trim());

    Ok(())
}

fn require_gemini_key() -> anyhow::Result<()> {
    let has_google = std::env::var("GOOGLE_API_KEY")
        .ok()
        .filter(|value| !value.is_empty())
        .is_some();
    let has_gemini = std::env::var("GEMINI_API_KEY")
        .ok()
        .filter(|value| !value.is_empty())
        .is_some();

    if has_google || has_gemini {
        return Ok(());
    }

    Err(anyhow!(
        "Set GOOGLE_API_KEY or GEMINI_API_KEY before running this example."
    ))
}

fn build_agent(
    model_string: &str,
    existing_messages: Option<Vec<Message>>,
) -> anyhow::Result<Agent> {
    let (provider, resolved_model) = from_model_string(model_string)
        .map_err(|e| anyhow!("failed to resolve {model_string}: {e}"))?;

    let mut builder = Agent::builder()
        .provider(provider)
        .tools(cersei::tools::none())
        .system_prompt("You are a concise assistant. Answer in exactly one sentence.")
        .model(&resolved_model)
        .max_turns(3)
        .max_tokens(512)
        .permission_policy(AllowAll)
        .working_dir(".");

    if let Some(messages) = existing_messages {
        builder = builder.with_messages(messages);
    }

    builder
        .build()
        .map_err(|e| anyhow!("failed to build agent for {model_string}: {e}"))
}
