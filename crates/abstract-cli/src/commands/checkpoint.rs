use super::CommandAction;
use cersei_tools::xfile_storage::create_checkpoint;

pub fn run(args: &str, session_id: &str) -> Result<CommandAction, String> {
    if !args.trim().is_empty() {
        return Err("Usage: /checkpoint".to_string());
    }

    let summary = create_checkpoint(session_id);
    if summary.tracked_files == 0 {
        eprintln!("\x1b[90m  Checkpoint saved: no tracked files yet\x1b[0m");
    } else {
        eprintln!(
            "\x1b[90m  Checkpoint saved for {} tracked file(s)\x1b[0m",
            summary.tracked_files
        );
    }

    Ok(CommandAction::InjectUserMessage {
        message: "User made checkpoint".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_returns_injected_user_message() {
        let session_id = format!("checkpoint-command-{}", uuid::Uuid::new_v4());

        let action = run("", &session_id).unwrap();
        match action {
            CommandAction::InjectUserMessage { message } => {
                assert_eq!(message, "User made checkpoint");
            }
            _ => panic!("unexpected command action"),
        }
    }
}
