use super::CommandAction;
use crate::config::AppConfig;
use anyhow::{bail, Context};
use std::process::Command;

pub async fn run(args: &str, config: &AppConfig) -> anyhow::Result<CommandAction> {
    let diff = match git_diff(config)? {
        Some(diff) => diff,
        None => {
            eprintln!("No changes to review.");
            return Ok(CommandAction::None);
        }
    };

    Ok(CommandAction::RunReviewer {
        diff,
        hint: args.trim().to_string(),
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
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_reviewer_action_contains_full_diff() {
        let action = CommandAction::RunReviewer {
            diff: "diff --git a/a.rs b/a.rs\n+hello\n".to_string(),
            hint: String::new(),
        };

        match action {
            CommandAction::RunReviewer { diff, .. } => {
                assert!(diff.contains("diff --git a/a.rs b/a.rs"));
                assert!(diff.contains("+hello"));
            }
            _ => panic!("unexpected action"),
        }
    }

    #[test]
    fn run_reviewer_action_carries_hint() {
        let action = CommandAction::RunReviewer {
            diff: "some diff".to_string(),
            hint: "focus on abc.rs".to_string(),
        };

        match action {
            CommandAction::RunReviewer { hint, .. } => {
                assert_eq!(hint, "focus on abc.rs");
            }
            _ => panic!("unexpected action"),
        }
    }
}
