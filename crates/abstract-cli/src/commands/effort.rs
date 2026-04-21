use super::CommandAction;
use crate::config::{self, AppConfig};

pub fn run(args: &str, config: &AppConfig) -> anyhow::Result<CommandAction> {
    let args = args.trim();
    if args.is_empty() {
        eprintln!("Current effort: {}", config.effort);
        eprintln!("\x1b[90mUsage: /effort <low|medium|high|max|tokens>\x1b[0m");
        eprintln!("\x1b[90mBudgets: low=1024, medium=4096, high=8192, max=32768 tokens\x1b[0m");
        return Ok(CommandAction::None);
    }

    let effort = config::parse_effort_budget(args).ok_or_else(|| {
        anyhow::anyhow!(
            "Unknown effort '{}'. Use /effort <low|medium|high|max|tokens>.",
            args
        )
    })?;

    Ok(CommandAction::SwitchEffort { effort })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_effort_levels() {
        assert_eq!(config::parse_effort_budget("low"), Some(1024));
        assert_eq!(config::parse_effort_budget("medium"), Some(4096));
        assert_eq!(config::parse_effort_budget("high"), Some(8192));
        assert_eq!(config::parse_effort_budget("max"), Some(32768));
    }

    #[test]
    fn parses_numeric_effort() {
        assert_eq!(config::parse_effort_budget("12345"), Some(12345));
    }

    #[test]
    fn rejects_unknown_effort() {
        assert_eq!(config::parse_effort_budget("extreme"), None);
        assert_eq!(config::parse_effort_budget("0"), None);
    }

    #[test]
    fn set_effort_returns_switch_action() {
        let action = run("high", &AppConfig::default()).unwrap();
        match action {
            CommandAction::SwitchEffort { effort } => assert_eq!(effort, 8192),
            _ => panic!("unexpected action"),
        }
    }

    #[test]
    fn set_numeric_effort_returns_switch_action() {
        let action = run("12000", &AppConfig::default()).unwrap();
        match action {
            CommandAction::SwitchEffort { effort } => assert_eq!(effort, 12000),
            _ => panic!("unexpected action"),
        }
    }
}
