pub fn should_display_agent(provider_id: &str, agent_name: &str) -> bool {
    if provider_id.eq_ignore_ascii_case("openai") {
        return agent_name.contains("5.4");
    }

    if provider_id.eq_ignore_ascii_case("google") {
        return matches!(
            agent_name,
            "gemini-3.1-pro-preview" | "gemini-3-flash-preview"
        );
    }

    if provider_id.eq_ignore_ascii_case("xai") {
        return matches!(
            agent_name,
            "grok-4.20-0309-non-reasoning" | "grok-4.20-0309-reasoning"
        );
    }

    true
}

pub fn filter_agent_names<I, S>(provider_id: &str, agent_names: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    agent_names
        .into_iter()
        .map(Into::into)
        .filter(|agent_name| should_display_agent(provider_id, agent_name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{filter_agent_names, should_display_agent};

    #[test]
    fn filters_openai_agents_without_5_4() {
        assert!(should_display_agent("openai", "assistant-5.4"));
        assert!(!should_display_agent("openai", "assistant-5.3"));
    }

    #[test]
    fn keeps_non_openai_agents() {
        assert!(should_display_agent("anthropic", "claude-sonnet-4-6"));
    }

    #[test]
    fn filters_google_agents_to_supported_allowlist() {
        assert!(should_display_agent("google", "gemini-3.1-pro-preview"));
        assert!(should_display_agent("google", "gemini-3-flash-preview"));
        assert!(!should_display_agent("google", "gemini-2.5-pro"));
        assert!(!should_display_agent("google", "gemini-experimental"));
    }

    #[test]
    fn filters_xai_agents_to_supported_allowlist() {
        assert!(should_display_agent("xai", "grok-4.20-0309-non-reasoning"));
        assert!(should_display_agent("xai", "grok-4.20-0309-reasoning"));
        assert!(!should_display_agent("xai", "grok-4-fast-reasoning"));
        assert!(!should_display_agent("xai", "grok-code-fast-1"));
    }

    #[test]
    fn filters_agent_name_lists() {
        let filtered = filter_agent_names("openai", ["agent-5.4", "agent-5.3", "agent-6.0"]);
        assert_eq!(filtered, vec!["agent-5.4"]);
    }

    #[test]
    fn filters_gpt_name_lists_as_expected() {
        let filtered = filter_agent_names("openai", ["gpt-5.4", "gpt-5.3", "gpt-4o"]);
        assert_eq!(filtered, vec!["gpt-5.4"]);
    }

    #[test]
    fn filters_xai_name_lists_as_expected() {
        let filtered = filter_agent_names(
            "xai",
            [
                "grok-4.20-0309-reasoning",
                "grok-4-fast-reasoning",
                "grok-4.20-0309-non-reasoning",
            ],
        );
        assert_eq!(
            filtered,
            vec!["grok-4.20-0309-reasoning", "grok-4.20-0309-non-reasoning"]
        );
    }
}
