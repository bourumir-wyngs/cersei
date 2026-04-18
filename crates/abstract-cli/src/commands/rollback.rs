use super::CommandAction;
use cersei_tools::xfile_storage::rollback_to_checkpoint;

pub async fn run(args: &str, session_id: &str) -> Result<CommandAction, String> {
    if !args.trim().is_empty() {
        return Err("Usage: /rollback".to_string());
    }

    let summary = rollback_to_checkpoint(session_id).await?;
    let checkpoint_kind = if summary.used_explicit_checkpoint {
        "saved checkpoint"
    } else {
        "implicit session-start baseline"
    };
    eprintln!(
        "\x1b[90m  Rolled back to the {}: changed {}, removed {}, unchanged {}\x1b[0m",
        checkpoint_kind, summary.changed_files, summary.removed_files, summary.unchanged_files
    );

    Ok(CommandAction::InjectUserMessage {
        message: "User rolled back".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cersei_tools::xfile_storage::{
        apply_file_to_disk, create_checkpoint, record_disk_state, store_written_text,
    };

    #[tokio::test]
    async fn rollback_restores_files_and_returns_injected_user_message() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("rollback-command-{}", uuid::Uuid::new_v4());
        let path = tmp.path().join("sample.txt");

        let first = store_written_text(&session_id, &path, "before\n");
        apply_file_to_disk(&path, &first.file).await.unwrap();
        record_disk_state(&session_id, &path).unwrap();
        create_checkpoint(&session_id);

        let second = store_written_text(&session_id, &path, "after\n");
        apply_file_to_disk(&path, &second.file).await.unwrap();
        record_disk_state(&session_id, &path).unwrap();

        let action = run("", &session_id).await.unwrap();

        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "before\n");
        match action {
            CommandAction::InjectUserMessage { message } => {
                assert_eq!(message, "User rolled back");
            }
            _ => panic!("unexpected command action"),
        }
    }
}
