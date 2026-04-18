//! System prompt assembly with CLI-specific context injection.

use crate::config::AppConfig;
use cersei_agent::system_prompt::{
    build_system_prompt, OutputStyle, SystemPromptOptions, SystemPromptPrefix,
};
use cersei_memory::manager::MemoryManager;

/// Build the complete system prompt for the CLI agent.
pub fn build_cli_system_prompt(config: &AppConfig, memory_manager: &MemoryManager) -> String {
    build_cli_system_prompt_with_append(config, memory_manager, None)
}

/// Build the reviewer-specific system prompt for the CLI reviewer agent.
pub fn build_cli_reviewer_system_prompt(
    config: &AppConfig,
    memory_manager: &MemoryManager,
) -> String {
    build_cli_system_prompt_with_append(config, memory_manager, Some(REVIEWER_APPEND_PROMPT))
}

fn build_cli_system_prompt_with_append(
    config: &AppConfig,
    memory_manager: &MemoryManager,
    append_system_prompt: Option<&str>,
) -> String {
    let memory_content = memory_manager.build_context();

    let mut extra_dynamic = Vec::new();

    // Current date/time
    let now = chrono::Local::now();
    extra_dynamic.push((
        "environment".to_string(),
        format!(
            "Date: {}\nOS: {} {}\nShell: {}\nWorking directory: {}",
            now.format("%Y-%m-%d %H:%M %Z"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::env::var("SHELL").unwrap_or_else(|_| "unknown".into()),
            config.working_dir.display(),
        ),
    ));

    // Git info (if available)
    if let Some(git_info) = detect_git_info(&config.working_dir) {
        extra_dynamic.push(("git".to_string(), git_info));
    }

    // Project instructions (.abstract/instructions.md)
    let mut extra_cached = Vec::new();
    let instructions_path = config.working_dir.join(".abstract").join("instructions.md");
    if instructions_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&instructions_path) {
            extra_cached.push(("project_instructions".to_string(), content));
        }
    }

    // AGENTS.md briefing
    let agents_md_path = config.working_dir.join("AGENTS.md");
    if agents_md_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&agents_md_path) {
            extra_cached.push(("agents_briefing".to_string(), content));
        }
    }

    let opts = SystemPromptOptions {
        prefix: Some(SystemPromptPrefix::Interactive),
        output_style: OutputStyle::from_str(&config.output_style),
        working_directory: Some(config.working_dir.display().to_string()),
        memory_content,
        extra_cached_sections: extra_cached,
        extra_dynamic_sections: extra_dynamic,
        append_system_prompt: append_system_prompt.map(str::to_string),
        ..Default::default()
    };

    build_system_prompt(&opts)
}

const REVIEWER_APPEND_PROMPT: &str = r#"
You are the reviewer agent.

You are reviewing diffs produced by another agent working in the same workspace.
You must not assume you have access to that agent's private reasoning or session
history unless it appears in your own reviewer session.

Focus on:
- major correctness defects
- unsafe code or security issues
- suspicious behavior or potentially bad intent
- regressions, missing validation, and risky side effects

Do not make edits, rollbacks, or other workspace changes. You may inspect current
files, git state, and file history to support the review.

Do not spend time on minor style nits unless they point to a real defect.
If you find no serious issues, say so clearly.
"#;

fn detect_git_info(working_dir: &std::path::Path) -> Option<String> {
    use std::process::Command;

    // Check if we're in a git repo
    let status = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(working_dir)
        .output()
        .ok()?;

    if !status.status.success() {
        return None;
    }

    let branch = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(working_dir)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "detached".into());

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(working_dir)
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let status_str = if dirty { " (dirty)" } else { "" };

    Some(format!("Branch: {branch}{status_str}"))
}
