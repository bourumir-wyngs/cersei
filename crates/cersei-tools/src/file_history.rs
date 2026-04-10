//! Versioned file history: tracks reads/writes/edits and stores content snapshots.
//!
//! Stored in `Extensions` as `Arc<FileHistory>`. File-mutating tools (Edit, Write)
//! call `snapshot_before_write` before modifying a file; the Read tool calls
//! `record_read`. The `FileHistoryTool` exposes revisions, diffs, and revert to
//! the AI.

use parking_lot::Mutex;
use similar::TextDiff;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A single revision of a file's content, captured *before* a mutation.
#[derive(Debug, Clone)]
pub struct Revision {
    /// 1-based revision number.
    pub number: u32,
    /// Full file content at this point in time.
    pub content: String,
    /// Unix timestamp when the snapshot was taken.
    pub timestamp: u64,
    /// What caused the snapshot: "edit", "write", "revert", or "restore".
    pub operation: String,
}

/// Per-file tracking state.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub read_count: u32,
    pub write_count: u32,
    pub edit_count: u32,
    pub last_accessed: u64,
    /// Chronological list of content snapshots (before each mutation).
    pub revisions: Vec<Revision>,
}

impl FileEntry {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            read_count: 0,
            write_count: 0,
            edit_count: 0,
            last_accessed: 0,
            revisions: Vec::new(),
        }
    }

    fn next_revision_number(&self) -> u32 {
        self.revisions.last().map_or(1, |r| r.number + 1)
    }
}

/// Shared, thread-safe file history store.
///
/// Insert into `Extensions` as `Arc<FileHistory>` so all tools can access it.
#[derive(Debug, Default)]
pub struct FileHistory {
    entries: Mutex<HashMap<PathBuf, FileEntry>>,
}

impl FileHistory {
    pub fn new() -> Self {
        Self::default()
    }

    fn normalize_key(path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
        } else {
            path.to_path_buf()
        }
    }

    // ── Recording helpers (called by Read / Edit / Write tools) ─────────

    /// Record that a file was read.
    pub fn record_read(&self, path: &PathBuf) {
        let key = Self::normalize_key(path);
        let mut entries = self.entries.lock();
        let entry = entries
            .entry(key.clone())
            .or_insert_with(|| FileEntry::new(key));
        entry.read_count += 1;
        entry.last_accessed = now_secs();
    }

    /// Capture a snapshot of the file's current content *before* a mutation.
    /// Returns the revision number assigned.
    pub fn snapshot_before_write(&self, path: &PathBuf, content: &str, operation: &str) -> u32 {
        let key = Self::normalize_key(path);
        let mut entries = self.entries.lock();
        let entry = entries
            .entry(key.clone())
            .or_insert_with(|| FileEntry::new(key));
        let rev = entry.next_revision_number();
        entry.revisions.push(Revision {
            number: rev,
            content: content.to_string(),
            timestamp: now_secs(),
            operation: operation.to_string(),
        });
        match operation {
            "edit" | "restore" | "revert" => entry.edit_count += 1,
            _ => entry.write_count += 1,
        }
        entry.last_accessed = now_secs();
        rev
    }

    // ── Query methods (used by FileHistoryTool) ─────────────────────────

    /// List all tracked files with summary info.
    pub fn list_files(&self) -> Vec<FileSummary> {
        let entries = self.entries.lock();
        let mut summaries: Vec<FileSummary> = entries
            .values()
            .map(|e| FileSummary {
                path: e.path.clone(),
                read_count: e.read_count,
                write_count: e.write_count,
                edit_count: e.edit_count,
                revision_count: e.revisions.len() as u32,
                last_accessed: e.last_accessed,
            })
            .collect();
        summaries.sort_by(|a, b| b.last_accessed.cmp(&a.last_accessed));
        summaries
    }

    /// Get the list of revisions for a file.
    pub fn get_revisions(&self, path: &PathBuf) -> Option<Vec<RevisionInfo>> {
        let key = Self::normalize_key(path);
        let entries = self.entries.lock();
        entries.get(&key).map(|e| {
            e.revisions
                .iter()
                .map(|r| RevisionInfo {
                    number: r.number,
                    timestamp: r.timestamp,
                    operation: r.operation.clone(),
                    size_bytes: r.content.len(),
                })
                .collect()
        })
    }

    /// Get the content of a specific revision.
    pub fn get_revision_content(&self, path: &PathBuf, revision: u32) -> Option<String> {
        let key = Self::normalize_key(path);
        let entries = self.entries.lock();
        entries.get(&key).and_then(|e| {
            e.revisions
                .iter()
                .find(|r| r.number == revision)
                .map(|r| r.content.clone())
        })
    }

    /// Compute a unified diff between two sources.
    /// Each source is either a revision number or "current" (meaning the file on disk).
    pub fn diff_revisions(
        &self,
        path: &PathBuf,
        from_rev: u32,
        to_content: &str,
        to_label: &str,
    ) -> Option<String> {
        let from_content = self.get_revision_content(path, from_rev)?;
        Some(unified_diff(
            &from_content,
            to_content,
            &format!("rev {}", from_rev),
            to_label,
        ))
    }

    /// Diff between two stored revisions.
    pub fn diff_two_revisions(&self, path: &PathBuf, from_rev: u32, to_rev: u32) -> Option<String> {
        let key = Self::normalize_key(path);
        let entries = self.entries.lock();
        let entry = entries.get(&key)?;
        let from = entry.revisions.iter().find(|r| r.number == from_rev)?;
        let to = entry.revisions.iter().find(|r| r.number == to_rev)?;
        Some(unified_diff(
            &from.content,
            &to.content,
            &format!("rev {}", from_rev),
            &format!("rev {}", to_rev),
        ))
    }

    /// Get the number of revisions for a file (0 if untracked).
    pub fn revision_count(&self, path: &PathBuf) -> u32 {
        let key = Self::normalize_key(path);
        let entries = self.entries.lock();
        entries.get(&key).map_or(0, |e| e.revisions.len() as u32)
    }

    pub fn file_count(&self) -> usize {
        self.entries.lock().len()
    }

    /// Build a context string for the system prompt.
    pub fn build_context(&self) -> Option<String> {
        let entries = self.entries.lock();
        let mut modified: Vec<&FileEntry> = entries
            .values()
            .filter(|f| f.write_count > 0 || f.edit_count > 0)
            .collect();
        if modified.is_empty() {
            return None;
        }
        modified.sort_by(|a, b| b.last_accessed.cmp(&a.last_accessed));

        let lines: Vec<String> = modified
            .iter()
            .take(20)
            .map(|f| {
                let ops = format!(
                    "{}{}{}",
                    if f.read_count > 0 {
                        format!("r{} ", f.read_count)
                    } else {
                        String::new()
                    },
                    if f.write_count > 0 {
                        format!("w{} ", f.write_count)
                    } else {
                        String::new()
                    },
                    if f.edit_count > 0 {
                        format!("e{}", f.edit_count)
                    } else {
                        String::new()
                    },
                );
                let rev_info = if f.revisions.is_empty() {
                    String::new()
                } else {
                    format!(", {} rev", f.revisions.len())
                };
                format!("- {} ({}{})", f.path.display(), ops.trim(), rev_info)
            })
            .collect();

        Some(format!(
            "Files modified this session:\n{}",
            lines.join("\n")
        ))
    }
}

// ─── Public summary types ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FileSummary {
    pub path: PathBuf,
    pub read_count: u32,
    pub write_count: u32,
    pub edit_count: u32,
    pub revision_count: u32,
    pub last_accessed: u64,
}

#[derive(Debug, Clone)]
pub struct RevisionInfo {
    pub number: u32,
    pub timestamp: u64,
    pub operation: String,
    pub size_bytes: usize,
}

// ─── Diff helper ────────────────────────────────────────────────────────────

pub(crate) fn unified_diff(old: &str, new: &str, old_label: &str, new_label: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = String::new();
    out.push_str(&format!("--- {}\n+++ {}\n", old_label, new_label));
    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        out.push_str(&format!("{}", hunk));
    }
    if out.ends_with('\n') {
        // trim trailing newline for cleaner output
    }
    out
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_read() {
        let history = FileHistory::new();
        let path = PathBuf::from("src/main.rs");
        history.record_read(&path);
        history.record_read(&path);

        let files = history.list_files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].read_count, 2);
        assert_eq!(files[0].revision_count, 0);
    }

    #[test]
    fn test_snapshot_and_revisions() {
        let history = FileHistory::new();
        let path = PathBuf::from("src/lib.rs");

        history.snapshot_before_write(&path, "version 1", "write");
        history.snapshot_before_write(&path, "version 2", "edit");

        let revs = history.get_revisions(&path).unwrap();
        assert_eq!(revs.len(), 2);
        assert_eq!(revs[0].number, 1);
        assert_eq!(revs[0].operation, "write");
        assert_eq!(revs[1].number, 2);
        assert_eq!(revs[1].operation, "edit");

        assert_eq!(history.get_revision_content(&path, 1).unwrap(), "version 1");
        assert_eq!(history.get_revision_content(&path, 2).unwrap(), "version 2");
    }

    #[test]
    fn test_diff_two_revisions() {
        let history = FileHistory::new();
        let path = PathBuf::from("file.txt");

        history.snapshot_before_write(&path, "line1\nline2\n", "write");
        history.snapshot_before_write(&path, "line1\nline2\nline3\n", "edit");

        let diff = history.diff_two_revisions(&path, 1, 2).unwrap();
        assert!(diff.contains("+line3"));
    }

    #[test]
    fn test_diff_revision_vs_current() {
        let history = FileHistory::new();
        let path = PathBuf::from("file.txt");

        history.snapshot_before_write(&path, "old content\n", "write");

        let diff = history
            .diff_revisions(&path, 1, "new content\n", "current")
            .unwrap();
        assert!(diff.contains("-old content"));
        assert!(diff.contains("+new content"));
    }

    #[test]
    fn test_build_context() {
        let history = FileHistory::new();
        history.snapshot_before_write(&PathBuf::from("a.rs"), "x", "edit");
        history.snapshot_before_write(&PathBuf::from("b.rs"), "y", "write");

        let ctx = history.build_context().unwrap();
        assert!(ctx.contains("a.rs"));
        assert!(ctx.contains("b.rs"));
        assert!(ctx.contains("1 rev"));
    }

    #[test]
    fn test_build_context_empty() {
        let history = FileHistory::new();
        assert!(history.build_context().is_none());

        // Read-only doesn't count as modified
        history.record_read(&PathBuf::from("file.txt"));
        assert!(history.build_context().is_none());
    }

    #[test]
    fn test_unified_diff_helper() {
        let diff = unified_diff("a\nb\n", "a\nc\n", "old", "new");
        assert!(diff.contains("--- old"));
        assert!(diff.contains("+++ new"));
        assert!(diff.contains("-b"));
        assert!(diff.contains("+c"));
    }

    #[test]
    fn test_file_count_and_revision_count() {
        let history = FileHistory::new();
        assert_eq!(history.file_count(), 0);
        assert_eq!(history.revision_count(&PathBuf::from("x")), 0);

        history.record_read(&PathBuf::from("a.rs"));
        assert_eq!(history.file_count(), 1);
        assert_eq!(history.revision_count(&PathBuf::from("a.rs")), 0);

        history.snapshot_before_write(&PathBuf::from("a.rs"), "v1", "edit");
        assert_eq!(history.revision_count(&PathBuf::from("a.rs")), 1);

        history.snapshot_before_write(&PathBuf::from("b.rs"), "v1", "write");
        assert_eq!(history.file_count(), 2);
    }

    #[test]
    fn test_nonexistent_path_returns_none() {
        let history = FileHistory::new();
        let path = PathBuf::from("nonexistent.rs");

        assert!(history.get_revisions(&path).is_none());
        assert!(history.get_revision_content(&path, 1).is_none());
        assert!(history.diff_two_revisions(&path, 1, 2).is_none());
        assert!(history.diff_revisions(&path, 1, "x", "current").is_none());
    }

    #[test]
    fn test_nonexistent_revision_returns_none() {
        let history = FileHistory::new();
        let path = PathBuf::from("file.rs");
        history.snapshot_before_write(&path, "v1", "edit");

        assert!(history.get_revision_content(&path, 99).is_none());
        assert!(history.diff_two_revisions(&path, 1, 99).is_none());
    }

    #[test]
    fn test_list_files_returns_all() {
        let history = FileHistory::new();
        history.snapshot_before_write(&PathBuf::from("a.rs"), "x", "write");
        history.snapshot_before_write(&PathBuf::from("b.rs"), "y", "write");

        let files = history.list_files();
        assert_eq!(files.len(), 2);
        let paths: Vec<_> = files.iter().map(|f| f.path.clone()).collect();
        assert!(paths.contains(&PathBuf::from("a.rs")));
        assert!(paths.contains(&PathBuf::from("b.rs")));
    }

    #[test]
    fn test_revert_operation_increments_edit_count() {
        let history = FileHistory::new();
        let path = PathBuf::from("file.rs");

        history.snapshot_before_write(&path, "before revert", "revert");
        let files = history.list_files();
        assert_eq!(files[0].edit_count, 1);
        assert_eq!(files[0].write_count, 0);
    }

    #[test]
    fn test_restore_operation_increments_edit_count() {
        let history = FileHistory::new();
        let path = PathBuf::from("file.rs");

        history.snapshot_before_write(&path, "before restore", "restore");
        let files = history.list_files();
        assert_eq!(files[0].edit_count, 1);
        assert_eq!(files[0].write_count, 0);
    }

    #[test]
    fn test_diff_identical_revisions() {
        let history = FileHistory::new();
        let path = PathBuf::from("file.txt");

        history.snapshot_before_write(&path, "same\n", "write");
        history.snapshot_before_write(&path, "same\n", "edit");

        let diff = history.diff_two_revisions(&path, 1, 2).unwrap();
        // No @@ hunk markers when content is identical
        assert!(!diff.contains("@@"));
    }
}
