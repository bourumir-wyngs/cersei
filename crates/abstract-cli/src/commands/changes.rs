use super::CommandAction;
use cersei_tools::xfile_storage::{diff_against_checkpoint, diff_files, XCheckpointDiffSummary};

pub fn run(args: &str, session_id: &str) -> Result<CommandAction, String> {
    if !args.trim().is_empty() {
        return Err("Usage: /changes".to_string());
    }

    let summary = diff_against_checkpoint(session_id)?;
    println!("{}", render_changes(&summary));
    Ok(CommandAction::None)
}

fn render_changes(summary: &XCheckpointDiffSummary) -> String {
    if summary.entries.is_empty() {
        let baseline = if summary.used_explicit_checkpoint {
            "saved checkpoint"
        } else {
            "implicit session-start baseline"
        };
        return format!(
            "No differences between the current tracked session state and the {}.",
            baseline
        );
    }

    let baseline = if summary.used_explicit_checkpoint {
        "saved checkpoint"
    } else {
        "implicit session-start baseline"
    };
    let mut out = format!(
        "Combined diff between the current tracked session state and the {}:\n\n",
        baseline
    );
    for (idx, entry) in summary.entries.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        if entry.baseline_file.path == entry.current_file.path {
            out.push_str(&format!("File: {}\n", entry.current_file.path.display()));
        } else {
            out.push_str(&format!(
                "File: {} -> {}\n",
                entry.baseline_file.path.display(),
                entry.current_file.path.display()
            ));
        }
        out.push_str(&diff_files(
            &entry.baseline_file,
            &entry.current_file,
            &format!("rev {}", entry.baseline_revision),
            &format!("rev {} (current)", entry.current_revision),
        ));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }

    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cersei_tools::xfile_storage::{
        apply_file_to_disk, clear_session_xfile_storage, create_checkpoint, record_disk_state,
        store_written_text,
    };

    #[tokio::test]
    async fn changes_renders_diff_against_explicit_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("changes-command-{}", uuid::Uuid::new_v4());
        let path = tmp.path().join("sample.txt");
        clear_session_xfile_storage(&session_id);

        let first = store_written_text(&session_id, &path, "before\n");
        apply_file_to_disk(&path, &first.file).await.unwrap();
        record_disk_state(&session_id, &path).unwrap();
        create_checkpoint(&session_id);

        let second = store_written_text(&session_id, &path, "after\n");
        apply_file_to_disk(&path, &second.file).await.unwrap();
        record_disk_state(&session_id, &path).unwrap();

        let rendered = render_changes(&diff_against_checkpoint(&session_id).unwrap());

        assert!(rendered.contains("saved checkpoint"));
        assert!(rendered.contains(&format!("File: {}", path.display())));
        assert!(rendered.contains("-before"));
        assert!(rendered.contains("+after"));
    }

    #[tokio::test]
    async fn changes_renders_empty_diff_against_implicit_session_start() {
        let session_id = format!("changes-command-{}", uuid::Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let rendered = render_changes(&diff_against_checkpoint(&session_id).unwrap());

        assert_eq!(
            rendered,
            "No differences between the current tracked session state and the implicit session-start baseline."
        );
    }

    #[tokio::test]
    async fn changes_command_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("changes-command-{}", uuid::Uuid::new_v4());
        let path = tmp.path().join("sample.txt");
        clear_session_xfile_storage(&session_id);

        let first = store_written_text(&session_id, &path, "before\n");
        apply_file_to_disk(&path, &first.file).await.unwrap();
        record_disk_state(&session_id, &path).unwrap();
        create_checkpoint(&session_id);

        let second = store_written_text(&session_id, &path, "after\n");
        apply_file_to_disk(&path, &second.file).await.unwrap();
        record_disk_state(&session_id, &path).unwrap();

        let action = run("", &session_id).unwrap();
        assert!(matches!(action, CommandAction::None));
    }
}
