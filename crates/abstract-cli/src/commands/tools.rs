use super::CommandAction;
use crate::config::AppConfig;
use cersei_tools::{PermissionLevel, ToolCategory, ToolInfo};
use std::fmt::Write;

const CATEGORY_ORDER: [ToolCategory; 8] = [
    ToolCategory::FileSystem,
    ToolCategory::Shell,
    ToolCategory::Testing,
    ToolCategory::Web,
    ToolCategory::Memory,
    ToolCategory::Orchestration,
    ToolCategory::Mcp,
    ToolCategory::Custom,
];

pub fn run(args: &str) -> anyhow::Result<CommandAction> {
    if !args.trim().is_empty() {
        anyhow::bail!("Usage: /tools");
    }

    Ok(CommandAction::ShowTools)
}

pub fn print_report(
    config: &AppConfig,
    coding_tools: &[ToolInfo],
    reviewer_tools: &[ToolInfo],
) -> anyhow::Result<()> {
    eprint!("{}", format_report(config, coding_tools, reviewer_tools));
    Ok(())
}

pub fn format_report(
    config: &AppConfig,
    coding_tools: &[ToolInfo],
    reviewer_tools: &[ToolInfo],
) -> String {
    let mut out = String::new();

    writeln!(out, "Tools available in this session").ok();
    writeln!(out).ok();
    write_tool_section(
        &mut out,
        "Coding agent",
        &config.model,
        "actual model-visible tools",
        coding_tools,
    );
    writeln!(out).ok();
    write_tool_section(
        &mut out,
        "Reviewer",
        &config.reviewer_model,
        "tools attached to reviewer runs",
        reviewer_tools,
    );

    if !config.mcp_servers.is_empty() {
        writeln!(out).ok();
        writeln!(
            out,
            "Configured MCP servers: {}",
            config
                .mcp_servers
                .iter()
                .map(|server| server.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
        .ok();
        writeln!(
            out,
            "MCP tools are not included above unless they are attached to the current agent."
        )
        .ok();
    }

    out
}

fn write_tool_section(
    out: &mut String,
    title: &str,
    model: &str,
    source_label: &str,
    tools: &[ToolInfo],
) {
    writeln!(out, "{title}: {model} ({} {source_label})", tools.len()).ok();

    if tools.is_empty() {
        writeln!(out, "  none").ok();
        return;
    }

    for category in CATEGORY_ORDER {
        let category_tools: Vec<&ToolInfo> = tools
            .iter()
            .filter(|tool| tool.category == category)
            .collect();
        if category_tools.is_empty() {
            continue;
        }

        writeln!(out, "  {}:", category_label(category)).ok();
        for tool in category_tools {
            writeln!(
                out,
                "    {:<18} {}",
                tool.name,
                permission_label(tool.permission_level)
            )
            .ok();
        }
    }
}

fn category_label(category: ToolCategory) -> &'static str {
    match category {
        ToolCategory::FileSystem => "File system",
        ToolCategory::Shell => "Shell/process",
        ToolCategory::Testing => "Testing",
        ToolCategory::Web => "Web/browser",
        ToolCategory::Memory => "Memory",
        ToolCategory::Orchestration => "Orchestration",
        ToolCategory::Mcp => "MCP",
        ToolCategory::Custom => "Custom",
    }
}

fn permission_label(level: PermissionLevel) -> &'static str {
    match level {
        PermissionLevel::None => "no approval",
        PermissionLevel::ReadOnly => "read-only",
        PermissionLevel::Write => "write",
        PermissionLevel::Execute => "execute",
        PermissionLevel::Dangerous => "dangerous",
        PermissionLevel::Forbidden => "forbidden",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, category: ToolCategory, permission_level: PermissionLevel) -> ToolInfo {
        ToolInfo {
            name: name.to_string(),
            description: String::new(),
            permission_level,
            category,
        }
    }

    #[test]
    fn tools_command_returns_show_tools_action() {
        let action = run("").unwrap();
        assert!(matches!(action, CommandAction::ShowTools));
    }

    #[test]
    fn tools_command_rejects_arguments() {
        let err = match run("verbose") {
            Ok(_) => panic!("expected /tools with arguments to fail"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("Usage: /tools"));
    }

    #[test]
    fn report_groups_coding_and_reviewer_tools() {
        let config = AppConfig::default();
        let coding = vec![
            tool("Read", ToolCategory::FileSystem, PermissionLevel::ReadOnly),
            tool("Bash", ToolCategory::Shell, PermissionLevel::Execute),
        ];
        let reviewer = vec![tool(
            "MemoryRecall",
            ToolCategory::Memory,
            PermissionLevel::None,
        )];

        let report = format_report(&config, &coding, &reviewer);

        assert!(report.contains("Coding agent"));
        assert!(report.contains("Reviewer"));
        assert!(report.contains("Read"));
        assert!(report.contains("Bash"));
        assert!(report.contains("MemoryRecall"));
        assert!(report.contains("read-only"));
        assert!(report.contains("no approval"));
    }
}
