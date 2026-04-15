//! Pure reconciler: re-attach XFile tags to a fresh disk snapshot.
//!
//! # Algorithm
//!
//! The syncer runs a two-stage diff so that duplicate lines (blank lines,
//! lone `}`, repeated `use …`, etc.) never cause a tag to be misassigned.
//!
//! **Stage 1 – Patience anchors.**  
//! `similar::Algorithm::Patience` is run over the full old/new line slices.
//! Every `Equal` op produced by patience is recorded as a confirmed anchor:
//! a (old_idx, new_idx) pair where we know the identity with certainty.
//!
//! **Stage 2 – Myers gap fill.**  
//! The anchors divide both sequences into a set of contiguous *gap* pairs
//! `(old_gap, new_gap)`.  Each gap that is not already fully covered by an
//! anchor is refined with `similar::Algorithm::Myers`.  This handles
//! insertions/deletions inside a region that patience left ambiguous.
//!
//! **Materialisation.**  
//! The merged op list is walked once to build the output `XFile`:
//! - `Equal`  → reuse the old `XLine` (same tag, same content).
//! - `Insert` → allocate a fresh tag for each new line.
//! - `Delete` → drop the old `XLine`; record it in the report.
//! - `Replace` → drop old lines, allocate fresh tags for new lines.

use crate::xfile_storage::{next_tag, XFile, XLine};
use similar::{capture_diff_slices, Algorithm, DiffOp};

// ── public surface ────────────────────────────────────────────────────────────

/// A single line-level change recorded in the sync report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncChange {
    /// A line was kept; its tag is unchanged.
    Kept { tag: String, content: String },
    /// A line was inserted (no prior tag).
    Inserted { tag: String, content: String },
    /// A line was deleted from the old file.
    Deleted { tag: String, content: String },
    /// An old line was replaced by one or more new lines.
    Replaced {
        old_tag: String,
        old_content: String,
        new_tag: String,
        new_content: String,
    },
}

/// Summary counters included in every [`SyncResult`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncStats {
    pub kept: usize,
    pub inserted: usize,
    pub deleted: usize,
    pub replaced: usize,
}

/// The value returned by [`sync_disk_snapshot`].
#[derive(Debug, Clone)]
pub struct SyncResult {
    /// The reconciled file with tags re-attached.
    pub file: XFile,
    /// Per-line change log.
    pub changes: Vec<SyncChange>,
    /// Aggregate counters.
    pub stats: SyncStats,
}

/// Reconcile `old` (the in-memory tagged file) with `disk_text` (the fresh
/// content read from disk after a human edit).
///
/// Returns a new [`XFile`] whose lines carry:
/// - the **original tag** for every line that survived unchanged,
/// - a **fresh tag** for every inserted or replaced line.
///
/// This function is **pure**: it allocates no I/O, writes nothing to storage,
/// and does not mutate `old`.
pub fn sync_disk_snapshot(old: &XFile, disk_text: &str) -> SyncResult {
    let old_lines: Vec<&str> = old.content.iter().map(|l| l.content.as_str()).collect();
    let new_lines: Vec<&str> = disk_text.lines().collect();

    let ops = two_stage_diff(&old_lines, &new_lines);
    materialise(old, &old_lines, &new_lines, &ops)
}

// ── two-stage diff ────────────────────────────────────────────────────────────

/// Run patience first, then fill each non-equal gap with Myers.
fn two_stage_diff<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<DiffOp> {
    // Stage 1: patience over the full sequences.
    let patience_ops = capture_diff_slices(Algorithm::Patience, old, new);

    // Stage 2: for every non-equal op from patience, re-run Myers on the
    // sub-slices so we get the finest-grained Equal/Insert/Delete/Replace
    // breakdown within that gap.
    let mut result: Vec<DiffOp> = Vec::with_capacity(patience_ops.len() * 2);

    for op in patience_ops {
        match op {
            DiffOp::Equal { .. } => result.push(op),
            _ => {
                let old_range = op.old_range();
                let new_range = op.new_range();
                let old_sub = &old[old_range.clone()];
                let new_sub = &new[new_range.clone()];

                if old_sub.is_empty() || new_sub.is_empty() {
                    // Pure insertion or pure deletion — Myers adds nothing.
                    result.push(op);
                } else {
                    // Run Myers on the gap, then re-base the indices.
                    let sub_ops = capture_diff_slices(Algorithm::Myers, old_sub, new_sub);
                    for sub_op in sub_ops {
                        result.push(rebase_op(sub_op, old_range.start, new_range.start));
                    }
                }
            }
        }
    }

    result
}

/// Shift the absolute indices of a `DiffOp` by `old_offset` / `new_offset`.
fn rebase_op(op: DiffOp, old_offset: usize, new_offset: usize) -> DiffOp {
    match op {
        DiffOp::Equal {
            old_index,
            new_index,
            len,
        } => DiffOp::Equal {
            old_index: old_index + old_offset,
            new_index: new_index + new_offset,
            len,
        },
        DiffOp::Delete {
            old_index,
            old_len,
            new_index,
        } => DiffOp::Delete {
            old_index: old_index + old_offset,
            old_len,
            new_index: new_index + new_offset,
        },
        DiffOp::Insert {
            old_index,
            new_index,
            new_len,
        } => DiffOp::Insert {
            old_index: old_index + old_offset,
            new_index: new_index + new_offset,
            new_len,
        },
        DiffOp::Replace {
            old_index,
            old_len,
            new_index,
            new_len,
        } => DiffOp::Replace {
            old_index: old_index + old_offset,
            old_len,
            new_index: new_index + new_offset,
            new_len,
        },
    }
}

// ── materialisation ───────────────────────────────────────────────────────────

fn materialise(old: &XFile, _old_lines: &[&str], new_lines: &[&str], ops: &[DiffOp]) -> SyncResult {
    let mut content: Vec<XLine> = Vec::with_capacity(new_lines.len());
    let mut changes: Vec<SyncChange> = Vec::new();
    let mut stats = SyncStats::default();

    for &op in ops {
        match op {
            // ── Equal: reuse old tag verbatim ─────────────────────────────
            DiffOp::Equal { old_index, len, .. } => {
                for i in 0..len {
                    let old_line = &old.content[old_index + i];
                    content.push(XLine {
                        content: old_line.content.clone(),
                        line_number: 0, // renumbered below
                        tag: old_line.tag.clone(),
                    });
                    changes.push(SyncChange::Kept {
                        tag: old_line.tag.clone(),
                        content: old_line.content.clone(),
                    });
                    stats.kept += 1;
                }
            }

            // ── Insert: allocate fresh tags ───────────────────────────────
            DiffOp::Insert {
                new_index, new_len, ..
            } => {
                for i in 0..new_len {
                    let text = new_lines[new_index + i].to_string();
                    let tag = next_tag();
                    content.push(XLine {
                        content: text.clone(),
                        line_number: 0,
                        tag: tag.clone(),
                    });
                    changes.push(SyncChange::Inserted { tag, content: text });
                    stats.inserted += 1;
                }
            }

            // ── Delete: drop old lines, record them ───────────────────────
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                for i in 0..old_len {
                    let old_line = &old.content[old_index + i];
                    changes.push(SyncChange::Deleted {
                        tag: old_line.tag.clone(),
                        content: old_line.content.clone(),
                    });
                    stats.deleted += 1;
                }
                // Nothing pushed to `content` — the lines are gone.
            }

            // ── Replace: drop old, allocate fresh tags for new ────────────
            //
            // We pair up old and new lines 1-to-1 as long as both exist,
            // recording each as a `Replaced` change (preserving the old tag
            // in the report so callers can trace provenance).  Surplus new
            // lines become plain insertions; surplus old lines become
            // deletions.
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                let paired = old_len.min(new_len);

                // Paired: one old → one new
                for i in 0..paired {
                    let old_line = &old.content[old_index + i];
                    let new_text = new_lines[new_index + i].to_string();
                    let new_tag = next_tag();
                    content.push(XLine {
                        content: new_text.clone(),
                        line_number: 0,
                        tag: new_tag.clone(),
                    });
                    changes.push(SyncChange::Replaced {
                        old_tag: old_line.tag.clone(),
                        old_content: old_line.content.clone(),
                        new_tag,
                        new_content: new_text,
                    });
                    stats.replaced += 1;
                }

                // Surplus old lines → deleted
                for i in paired..old_len {
                    let old_line = &old.content[old_index + i];
                    changes.push(SyncChange::Deleted {
                        tag: old_line.tag.clone(),
                        content: old_line.content.clone(),
                    });
                    stats.deleted += 1;
                }

                // Surplus new lines → inserted
                for i in paired..new_len {
                    let text = new_lines[new_index + i].to_string();
                    let tag = next_tag();
                    content.push(XLine {
                        content: text.clone(),
                        line_number: 0,
                        tag: tag.clone(),
                    });
                    changes.push(SyncChange::Inserted { tag, content: text });
                    stats.inserted += 1;
                }
            }
        }
    }

    // Renumber lines 1-based.
    for (idx, line) in content.iter_mut().enumerate() {
        line.line_number = idx + 1;
    }

    SyncResult {
        file: XFile {
            path: old.path.clone(),
            content,
        },
        changes,
        stats,
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xfile_storage::store_written_text;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn make_xfile(text: &str) -> XFile {
        let session_id = format!("sync-test-{}", Uuid::new_v4());
        let path = PathBuf::from("/tmp/sync_test.txt");
        store_written_text(&session_id, &path, text).file
    }

    // ── identity ──────────────────────────────────────────────────────────────

    #[test]
    fn unchanged_file_keeps_all_tags() {
        let old = make_xfile("alpha\nbeta\ngamma\n");
        let result = sync_disk_snapshot(&old, "alpha\nbeta\ngamma");

        assert_eq!(result.file.content.len(), 3);
        assert_eq!(result.file.content[0].tag, old.content[0].tag);
        assert_eq!(result.file.content[1].tag, old.content[1].tag);
        assert_eq!(result.file.content[2].tag, old.content[2].tag);
        assert_eq!(result.stats.kept, 3);
        assert_eq!(result.stats.inserted, 0);
        assert_eq!(result.stats.deleted, 0);
        assert_eq!(result.stats.replaced, 0);
    }

    // ── single insertion ──────────────────────────────────────────────────────

    #[test]
    fn single_line_inserted_in_middle() {
        let old = make_xfile("alpha\ngamma\n");
        let result = sync_disk_snapshot(&old, "alpha\nbeta\ngamma");

        assert_eq!(result.file.content.len(), 3);
        // alpha: kept
        assert_eq!(result.file.content[0].tag, old.content[0].tag);
        assert_eq!(result.file.content[0].content, "alpha");
        // beta: fresh tag
        assert_ne!(result.file.content[1].tag, old.content[0].tag);
        assert_ne!(result.file.content[1].tag, old.content[1].tag);
        assert_eq!(result.file.content[1].content, "beta");
        // gamma: kept
        assert_eq!(result.file.content[2].tag, old.content[1].tag);
        assert_eq!(result.file.content[2].content, "gamma");

        assert_eq!(result.stats.kept, 2);
        assert_eq!(result.stats.inserted, 1);
        assert_eq!(result.stats.deleted, 0);
    }

    #[test]
    fn single_lines_inserted_at_file_boundaries() {
        let old = make_xfile("beta\ngamma\n");
        let result = sync_disk_snapshot(&old, "alpha\nbeta\ngamma\ndelta");

        assert_eq!(result.file.content.len(), 4);
        assert_eq!(result.file.content[1].tag, old.content[0].tag);
        assert_eq!(result.file.content[2].tag, old.content[1].tag);
        assert_eq!(result.file.content[0].content, "alpha");
        assert_eq!(result.file.content[3].content, "delta");
        assert_ne!(result.file.content[0].tag, old.content[0].tag);
        assert_ne!(result.file.content[3].tag, old.content[1].tag);

        assert_eq!(result.stats.kept, 2);
        assert_eq!(result.stats.inserted, 2);
        assert_eq!(result.stats.deleted, 0);
        assert_eq!(result.stats.replaced, 0);
    }

    // ── single deletion ───────────────────────────────────────────────────────

    #[test]
    fn single_line_deleted_from_middle() {
        let old = make_xfile("alpha\nbeta\ngamma\n");
        let result = sync_disk_snapshot(&old, "alpha\ngamma");

        assert_eq!(result.file.content.len(), 2);
        assert_eq!(result.file.content[0].tag, old.content[0].tag);
        assert_eq!(result.file.content[1].tag, old.content[2].tag);

        assert_eq!(result.stats.kept, 2);
        assert_eq!(result.stats.deleted, 1);

        let deleted = result
            .changes
            .iter()
            .find(|c| matches!(c, SyncChange::Deleted { content, .. } if content == "beta"));
        assert!(deleted.is_some());
    }

    // ── single replacement ────────────────────────────────────────────────────

    #[test]
    fn single_line_replaced() {
        let old = make_xfile("alpha\nbeta\ngamma\n");
        let result = sync_disk_snapshot(&old, "alpha\nBETA\ngamma");

        assert_eq!(result.file.content.len(), 3);
        // alpha and gamma survive
        assert_eq!(result.file.content[0].tag, old.content[0].tag);
        assert_eq!(result.file.content[2].tag, old.content[2].tag);
        // BETA gets a fresh tag
        assert_ne!(result.file.content[1].tag, old.content[1].tag);
        assert_eq!(result.file.content[1].content, "BETA");

        assert_eq!(result.stats.replaced, 1);
        assert_eq!(result.stats.kept, 2);
    }

    #[test]
    fn replace_hunk_that_expands_tracks_replaced_and_inserted_lines() {
        let old = make_xfile("alpha\nbeta\ngamma\n");
        let result = sync_disk_snapshot(&old, "alpha\nBETA-1\nBETA-2\ngamma");

        assert_eq!(result.file.content.len(), 4);
        assert_eq!(result.file.content[0].tag, old.content[0].tag);
        assert_eq!(result.file.content[3].tag, old.content[2].tag);
        assert_eq!(result.file.content[1].content, "BETA-1");
        assert_eq!(result.file.content[2].content, "BETA-2");
        assert_ne!(result.file.content[1].tag, old.content[1].tag);
        assert_ne!(result.file.content[2].tag, old.content[1].tag);

        assert_eq!(result.stats.kept, 2);
        assert_eq!(result.stats.replaced, 1);
        assert_eq!(result.stats.inserted, 1);
        assert_eq!(result.stats.deleted, 0);

        assert!(result.changes.iter().any(|change| {
            matches!(
                change,
                SyncChange::Replaced {
                    old_tag,
                    old_content,
                    new_content,
                    ..
                } if old_tag == &old.content[1].tag
                    && old_content == "beta"
                    && new_content == "BETA-1"
            )
        }));
        assert!(result.changes.iter().any(|change| {
            matches!(
                change,
                SyncChange::Inserted { content, .. } if content == "BETA-2"
            )
        }));
    }

    #[test]
    fn replace_hunk_that_shrinks_tracks_replaced_and_deleted_lines() {
        let old = make_xfile("alpha\nbeta\ngamma\ndelta\n");
        let result = sync_disk_snapshot(&old, "alpha\nBETA-GAMMA\ndelta");

        assert_eq!(result.file.content.len(), 3);
        assert_eq!(result.file.content[0].tag, old.content[0].tag);
        assert_eq!(result.file.content[2].tag, old.content[3].tag);
        assert_eq!(result.file.content[1].content, "BETA-GAMMA");
        assert_ne!(result.file.content[1].tag, old.content[1].tag);
        assert_ne!(result.file.content[1].tag, old.content[2].tag);

        assert_eq!(result.stats.kept, 2);
        assert_eq!(result.stats.replaced, 1);
        assert_eq!(result.stats.deleted, 1);
        assert_eq!(result.stats.inserted, 0);

        assert!(result.changes.iter().any(|change| {
            matches!(
                change,
                SyncChange::Replaced {
                    old_tag,
                    old_content,
                    new_content,
                    ..
                } if old_tag == &old.content[1].tag
                    && old_content == "beta"
                    && new_content == "BETA-GAMMA"
            )
        }));
        assert!(result.changes.iter().any(|change| {
            matches!(
                change,
                SyncChange::Deleted { tag, content } if tag == &old.content[2].tag
                    && content == "gamma"
            )
        }));
    }

    // ── duplicate lines ───────────────────────────────────────────────────────

    #[test]
    fn duplicate_lines_are_matched_in_sequence() {
        // The file has repeated `}` lines. The syncer must not confuse them.
        let old = make_xfile("fn foo() {\n    1\n}\nfn bar() {\n    2\n}\n");
        // Human deleted `fn bar` block entirely.
        let result = sync_disk_snapshot(&old, "fn foo() {\n    1\n}");

        assert_eq!(result.file.content.len(), 3);
        // The first `}` (index 2) must survive with its original tag.
        assert_eq!(result.file.content[2].tag, old.content[2].tag);
        assert_eq!(result.stats.deleted, 3); // fn bar, 2, }
    }

    #[test]
    fn duplicate_runs_preserve_left_to_right_tags_across_insertions() {
        let old = make_xfile("start\nrepeat\nrepeat\nend\n");
        let result = sync_disk_snapshot(&old, "start\nrepeat\nmiddle\nrepeat\nend");

        assert_eq!(result.file.content.len(), 5);
        assert_eq!(result.file.content[0].tag, old.content[0].tag);
        assert_eq!(result.file.content[1].tag, old.content[1].tag);
        assert_eq!(result.file.content[3].tag, old.content[2].tag);
        assert_eq!(result.file.content[4].tag, old.content[3].tag);
        assert_eq!(result.file.content[2].content, "middle");
        assert_ne!(result.file.content[2].tag, old.content[1].tag);
        assert_ne!(result.file.content[2].tag, old.content[2].tag);

        assert_eq!(result.stats.kept, 4);
        assert_eq!(result.stats.inserted, 1);
        assert_eq!(result.stats.deleted, 0);
        assert_eq!(result.stats.replaced, 0);
    }

    // ── line numbers ──────────────────────────────────────────────────────────

    #[test]
    fn line_numbers_are_renumbered_correctly() {
        let old = make_xfile("a\nb\nc\n");
        let result = sync_disk_snapshot(&old, "a\nX\nc");

        for (idx, line) in result.file.content.iter().enumerate() {
            assert_eq!(line.line_number, idx + 1);
        }
    }

    // ── empty file ────────────────────────────────────────────────────────────

    #[test]
    fn empty_new_file_deletes_everything() {
        let old = make_xfile("a\nb\n");
        let result = sync_disk_snapshot(&old, "");

        assert!(result.file.content.is_empty());
        assert_eq!(result.stats.deleted, 2);
        assert_eq!(result.stats.kept, 0);
    }

    #[test]
    fn empty_old_file_inserts_everything() {
        let old = make_xfile("");
        let result = sync_disk_snapshot(&old, "a\nb");

        assert_eq!(result.file.content.len(), 2);
        assert_eq!(result.stats.inserted, 2);
        assert_eq!(result.stats.kept, 0);
    }

    // ── report integrity ──────────────────────────────────────────────────────

    #[test]
    fn sync_report_covers_every_new_line() {
        let old = make_xfile("one\ntwo\nthree\n");
        let result = sync_disk_snapshot(&old, "one\nTWO\nthree\nfour");

        // Every line in the output file must appear in the changes list.
        for line in &result.file.content {
            let found = result.changes.iter().any(|c| match c {
                SyncChange::Kept { tag, .. } => tag == &line.tag,
                SyncChange::Inserted { tag, .. } => tag == &line.tag,
                SyncChange::Replaced { new_tag, .. } => new_tag == &line.tag,
                SyncChange::Deleted { .. } => false,
            });
            assert!(found, "line tag {} not found in changes", line.tag);
        }
    }
}
