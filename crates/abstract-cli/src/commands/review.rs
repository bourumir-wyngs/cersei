use super::CommandAction;
use crate::config::AppConfig;
use anyhow::{bail, Context};
use std::process::Command;

const REVIEW_INSTRUCTION: &str =
    "Review these changes. You can also use tools to access current version of code.";

pub async fn run(config: &AppConfig) -> anyhow::Result<CommandAction> {
    let diff = match git_diff(config)? {
        Some(diff) => diff,
        None => {
            eprintln!("No changes to review.");
            return Ok(CommandAction::None);
        }
    };

    Ok(CommandAction::RunPrompt {
        prompt: build_review_prompt(&diff),
    })
}

fn git_diff(config: &AppConfig) -> anyhow::Result<Option<String>> {
    let output = Command::new("git")
        .args(["diff"])
        .current_dir(&config.working_dir)
        .output()
        .with_context(|| {
            format!(
                "Failed to run `git diff` in {}",
                config.working_dir.display()
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            bail!("`git diff` failed with status {}", output.status);
        }
        bail!("`git diff` failed: {stderr}");
    }

    let diff = String::from_utf8_lossy(&output.stdout).into_owned();
    if diff.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(diff))
    }
}

fn build_review_prompt(diff: &str) -> String {
    format!("{REVIEW_INSTRUCTION}\n\n{diff}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_review_prompt_includes_instruction_and_full_diff() {
        let diff = "diff --git a/a.rs b/a.rs\n+hello\n";
        let prompt = build_review_prompt(diff);

        assert!(prompt.starts_with(REVIEW_INSTRUCTION));
        assert!(prompt.ends_with(diff));
        assert!(prompt.contains("\n\ndiff --git a/a.rs b/a.rs\n+hello\n"));
    }
}
