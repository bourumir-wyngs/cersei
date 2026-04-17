//! Read-only Git tool powered by the `gix` Rust-native library.
//!
//! All operations are strictly read-only — no mutating gix APIs are called.
//! Safety is enforced at the API design layer: only read-path functions are
//! exposed and no write/commit/reset/checkout entry points exist in this module.
//!
//! Exposed as a single [`GitTool`] with a `command` dispatch parameter:
//!   - `log`           — commit history (reverse-chronological)
//!   - `show`          — single-commit metadata + file-change list
//!   - `diff_commits`  — unified diff between two revisions
//!   - `diff_worktree` — unified diff of uncommitted local changes
//!   - `read_file`     — file contents at a specific revision

use crate::{PermissionLevel, Tool, ToolCategory, ToolContext, ToolResult};
use async_trait::async_trait;
use gix::bstr::ByteSlice as _;
use serde::Deserialize;
use serde_json::Value;
use similar::TextDiff;
use std::path::{Path, PathBuf};

// ── constants ─────────────────────────────────────────────────────────────────

/// Default maximum diff lines before truncation. Callers may override via `max_diff_lines`.
const DEFAULT_MAX_DIFF_LINES: usize = 800;
/// Binary-detection threshold: if the first N bytes contain a NUL, treat as binary.
const BINARY_SNIFF_LEN: usize = 8000;

// ── private helpers ───────────────────────────────────────────────────────────

fn open_ts_repo(
    ctx_dir: &Path,
    override_path: Option<&str>,
) -> Result<gix::ThreadSafeRepository, String> {
    let path = match override_path {
        Some(p) => PathBuf::from(p),
        None => ctx_dir.to_path_buf(),
    };
    // `discover` walks up the directory tree to locate the repo root, so it works
    // when `path` is any subdirectory of the repository (not just the root itself).
    gix::ThreadSafeRepository::discover(&path)
        .map_err(|e| format!("cannot open git repo at {}: {e}", path.display()))
}

fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(BINARY_SNIFF_LEN)].contains(&0u8)
}

fn unified_diff_modify(rel_path: &str, old: &[u8], new: &[u8]) -> String {
    if is_binary(old) || is_binary(new) {
        return format!("diff --git a/{rel_path} b/{rel_path}\nBinary files differ\n");
    }
    let old_str = String::from_utf8_lossy(old);
    let new_str = String::from_utf8_lossy(new);
    TextDiff::from_lines(old_str.as_ref(), new_str.as_ref())
        .unified_diff()
        .header(&format!("a/{rel_path}"), &format!("b/{rel_path}"))
        .to_string()
}

fn unified_diff_add(rel_path: &str, data: &[u8]) -> String {
    if is_binary(data) {
        return format!("diff --git a/{rel_path} b/{rel_path}\nnew file\nBinary file\n");
    }
    let new_str = String::from_utf8_lossy(data);
    TextDiff::from_lines("", new_str.as_ref())
        .unified_diff()
        .header("/dev/null", &format!("b/{rel_path}"))
        .to_string()
}

fn unified_diff_delete(rel_path: &str, data: &[u8]) -> String {
    if is_binary(data) {
        return format!("diff --git a/{rel_path} b/{rel_path}\ndeleted file\nBinary file\n");
    }
    let old_str = String::from_utf8_lossy(data);
    TextDiff::from_lines(old_str.as_ref(), "")
        .unified_diff()
        .header(&format!("a/{rel_path}"), "/dev/null")
        .to_string()
}

/// Return the flat recursive list of blob entries `(repo-relative-path, oid)` for a tree.
/// Used only by `diff_worktree` which compares HEAD tree against on-disk files.
fn tree_blobs(
    repo: &gix::Repository,
    tree_id: gix::ObjectId,
) -> Result<Vec<(gix::bstr::BString, gix::ObjectId)>, String> {
    use gix::object::tree::EntryKind;
    use gix::traverse::tree::Recorder;

    let tree = repo
        .find_object(tree_id)
        .map_err(|e| e.to_string())?
        .peel_to_tree()
        .map_err(|e| e.to_string())?;

    let mut recorder = Recorder::default();
    tree.traverse()
        .breadthfirst(&mut recorder)
        .map_err(|e| e.to_string())?;

    Ok(recorder
        .records
        .into_iter()
        .filter(|e| matches!(e.mode.kind(), EntryKind::Blob | EntryKind::BlobExecutable))
        .map(|e| (e.filepath, e.oid))
        .collect())
}

/// Produce a unified patch between two trees using `repo.diff_tree_to_tree()`.
///
/// This delegates to gix's proper diff engine, which gives us:
///   - Rename / copy detection (50 % similarity by default, same as git)
///   - Mode-change headers (executable-bit flips)
///   - Alphabetically ordered output (git tree order)
///   - Correct binary detection via the diff pipeline
fn diff_trees(
    repo: &gix::Repository,
    old_tree: Option<&gix::Tree<'_>>,
    new_tree: Option<&gix::Tree<'_>>,
    max_lines: usize,
) -> Result<String, String> {
    use gix::bstr::ByteSlice as _;
    use gix::object::tree::diff::ChangeDetached;

    let opts = gix::diff::Options::default().with_rewrites(Some(gix::diff::Rewrites::default()));

    let changes = repo
        .diff_tree_to_tree(old_tree, new_tree, opts)
        .map_err(|e| e.to_string())?;

    let mut output = String::new();
    let mut total_lines = 0usize;
    let truncation_msg = format!(
        "[diff truncated at {max_lines} lines — increase max_diff_lines or use include_diff=false]\n"
    );

    let append = |patch: String, out: &mut String, lines: &mut usize| -> bool {
        *lines += patch.lines().count();
        out.push_str(&patch);
        if *lines > max_lines {
            out.push_str(&truncation_msg);
            true // truncated
        } else {
            false
        }
    };

    for change in &changes {
        let patch = match change {
            ChangeDetached::Addition {
                location,
                entry_mode,
                id,
                ..
            } => {
                if !entry_mode.is_blob() {
                    continue;
                }
                let rel = location.to_str_lossy();
                let data = repo
                    .find_object(*id)
                    .map_err(|e| e.to_string())?
                    .data
                    .to_vec();
                format!(
                    "diff --git a/{rel} b/{rel}\nnew file mode {}\n{}",
                    entry_mode.kind().as_octal_str(),
                    unified_diff_add(&rel, &data),
                )
            }

            ChangeDetached::Deletion {
                location,
                entry_mode,
                id,
                ..
            } => {
                if !entry_mode.is_blob() {
                    continue;
                }
                let rel = location.to_str_lossy();
                let data = repo
                    .find_object(*id)
                    .map_err(|e| e.to_string())?
                    .data
                    .to_vec();
                format!(
                    "diff --git a/{rel} b/{rel}\ndeleted file mode {}\n{}",
                    entry_mode.kind().as_octal_str(),
                    unified_diff_delete(&rel, &data),
                )
            }

            ChangeDetached::Modification {
                location,
                previous_entry_mode,
                previous_id,
                entry_mode,
                id,
                ..
            } => {
                if !entry_mode.is_blob() && !previous_entry_mode.is_blob() {
                    continue;
                }
                let rel = location.to_str_lossy();
                let mut header = format!("diff --git a/{rel} b/{rel}\n");
                // Mode change (e.g. +x bit)
                if previous_entry_mode != entry_mode {
                    header.push_str(&format!(
                        "old mode {}\nnew mode {}\n",
                        previous_entry_mode.kind().as_octal_str(),
                        entry_mode.kind().as_octal_str(),
                    ));
                }
                // Content change
                if previous_id != id {
                    let old = repo
                        .find_object(*previous_id)
                        .map_err(|e| e.to_string())?
                        .data
                        .to_vec();
                    let new = repo
                        .find_object(*id)
                        .map_err(|e| e.to_string())?
                        .data
                        .to_vec();
                    header.push_str(&unified_diff_modify(&rel, &old, &new));
                }
                header
            }

            ChangeDetached::Rewrite {
                source_location,
                source_id,
                location,
                id,
                entry_mode,
                copy,
                diff: line_stats,
                ..
            } => {
                if !entry_mode.is_blob() {
                    continue;
                }
                let src = source_location.to_str_lossy();
                let dst = location.to_str_lossy();
                let verb = if *copy { "copy" } else { "rename" };
                // Use similarity from gix's line-stat computation; fall back to
                // 100% when ids are identical (no line diff needed).
                let sim_pct = line_stats
                    .map(|s| (s.similarity * 100.0) as u8)
                    .unwrap_or(100);
                let mut patch = format!(
                    "diff --git a/{src} b/{dst}\n\
                     similarity index {sim_pct}%\n\
                     {verb} from {src}\n\
                     {verb} to {dst}\n",
                );
                // Content diff only when blobs differ
                if source_id != id {
                    let old = repo
                        .find_object(*source_id)
                        .map_err(|e| e.to_string())?
                        .data
                        .to_vec();
                    let new = repo
                        .find_object(*id)
                        .map_err(|e| e.to_string())?
                        .data
                        .to_vec();
                    patch.push_str(&unified_diff_modify(&dst, &old, &new));
                }
                patch
            }
        };

        if append(patch, &mut output, &mut total_lines) {
            return Ok(output);
        }
    }

    if output.is_empty() {
        Ok("(no differences)".into())
    } else {
        Ok(output)
    }
}

fn resolve_rev(repo: &gix::Repository, rev: &str) -> Result<gix::ObjectId, String> {
    repo.rev_parse_single(rev)
        .map(|id| id.detach())
        .map_err(|e| format!("cannot resolve revision '{rev}': {e}"))
}

fn fmt_time(seconds: gix::date::SecondsSinceUnixEpoch) -> String {
    use chrono::{TimeZone as _, Utc};
    Utc.timestamp_opt(seconds, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| seconds.to_string())
}

// ── input schema ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct GitInput {
    /// Which operation to run.
    command: String,

    // ── shared ────────────────────────────────────────────────────────────────
    /// Path to any file or directory inside the target repository.
    /// Defaults to the agent working directory.
    repo_path: Option<String>,

    // ── log ───────────────────────────────────────────────────────────────────
    /// Starting revision for `log` (default: `"HEAD"`).
    revision: Option<String>,
    /// Maximum commits to return for `log` (default 20, max 200).
    limit: Option<usize>,

    // ── show ──────────────────────────────────────────────────────────────────
    /// Commit hash / revspec for `show`.
    // (re-uses `revision`)
    /// Include full unified diff in `show` output (default false — file list only).
    include_diff: Option<bool>,

    // ── diff_commits ──────────────────────────────────────────────────────────
    /// Base (older) revision for `diff_commits`.
    old_rev: Option<String>,
    /// Target (newer) revision for `diff_commits`.
    new_rev: Option<String>,

    // ── diff_worktree ─────────────────────────────────────────────────────────
    /// Return a file list only (no patch text) for `diff_worktree` (default false).
    summary_only: Option<bool>,

    // ── read_file ─────────────────────────────────────────────────────────────
    /// Repository-relative file path for `read_file` (e.g. `"src/lib.rs"`).
    path: Option<String>,

    // ── diff controls ─────────────────────────────────────────────────────────
    /// Maximum number of diff lines before the patch is truncated.
    /// Default: 800. Increase for large changesets, decrease to save context.
    max_diff_lines: Option<usize>,
}

// ── GitTool ───────────────────────────────────────────────────────────────────

pub struct GitTool;

#[async_trait]
impl Tool for GitTool {
    fn name(&self) -> &str {
        "Git"
    }

    fn description(&self) -> &str {
        "Read-only Git browser. All commands are safe — no repository state is modified.\n\
         \n\
         Commands:\n\
         • log           — commit history: id, author, date, subject\n\
         • show          — single commit metadata + changed-file list (add include_diff=true for patch)\n\
         • diff_commits  — unified diff between old_rev and new_rev\n\
         • diff_worktree — uncommitted changes: modified/deleted tracked files + untracked files (gitignore-aware)\n\
         • read_file     — file contents at a specific revision\n\
         • status        — working tree status: staged, modified, untracked, deleted files + current branch\n\
         \n\
         Diff output is truncated at max_diff_lines (default 800). \
         Raise the limit or use summary_only=true to avoid truncation on large changesets."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Operation to perform.",
                    "enum": ["log", "show", "diff_commits", "diff_worktree", "read_file", "status"]
                },
                "repo_path": {
                    "type": "string",
                    "description": "Path to any file or directory inside the target repository (default: agent working directory)"
                },
                "revision": {
                    "type": "string",
                    "description": "[log, show] Revision expression — branch, tag, or commit hash. Defaults to HEAD."
                },
                "limit": {
                    "type": "integer",
                    "description": "[log] Maximum commits to return (default 20, max 200)",
                    "minimum": 1,
                    "maximum": 200
                },
                "include_diff": {
                    "type": "boolean",
                    "description": "[show] Include full unified diff in output (default false — returns file list only)"
                },
                "old_rev": {
                    "type": "string",
                    "description": "[diff_commits] Base (older) revision"
                },
                "new_rev": {
                    "type": "string",
                    "description": "[diff_commits] Target (newer) revision"
                },
                "summary_only": {
                    "type": "boolean",
                    "description": "[diff_worktree] Return a file list only, no patch content (default false)"
                },
                "path": {
                    "type": "string",
                    "description": "[read_file] Repository-relative file path (e.g. src/main.rs)"
                },
                "max_diff_lines": {
                    "type": "integer",
                    "description": "[show, diff_commits, diff_worktree] Maximum diff lines before truncation (default 800)",
                    "minimum": 1
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let input: GitInput = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("invalid input: {e}")),
        };

        let working_dir = ctx.working_dir.clone();

        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let ts_repo = open_ts_repo(&working_dir, input.repo_path.as_deref())?;
            let repo = ts_repo.to_thread_local();
            let max_lines = input.max_diff_lines.unwrap_or(DEFAULT_MAX_DIFF_LINES).max(1);

            match input.command.as_str() {
                "log" => cmd_log(&repo, input.revision.as_deref(), input.limit),

                "show" => cmd_show(
                    &repo,
                    input.revision.as_deref().unwrap_or("HEAD"),
                    input.include_diff.unwrap_or(false),
                    max_lines,
                ),

                "diff_commits" => {
                    let old_rev = input.old_rev.as_deref().ok_or("diff_commits requires old_rev")?;
                    let new_rev = input.new_rev.as_deref().ok_or("diff_commits requires new_rev")?;
                    cmd_diff_commits(&repo, old_rev, new_rev, max_lines)
                }

                "diff_worktree" => cmd_diff_worktree(
                    &repo,
                    input.summary_only.unwrap_or(false),
                    max_lines,
                ),

                "read_file" => {
                    let rev = input.revision.as_deref().unwrap_or("HEAD");
                    let path = input.path.as_deref().ok_or("read_file requires path")?;
                    cmd_read_file(&repo, rev, path)
                }

                "status" => cmd_status(&repo),

                other => Err(format!(
                    "unknown command '{other}'; valid commands: log, show, diff_commits, diff_worktree, read_file, status"
                )),
            }
        })
        .await;

        match result {
            Ok(Ok(content)) => ToolResult::success(content),
            Ok(Err(e)) => ToolResult::error(e),
            Err(e) => ToolResult::error(format!("task panicked: {e}")),
        }
    }
}

// ── command implementations ───────────────────────────────────────────────────

fn cmd_log(
    repo: &gix::Repository,
    revision: Option<&str>,
    limit: Option<usize>,
) -> Result<String, String> {
    let tip = resolve_rev(repo, revision.unwrap_or("HEAD"))?;
    let limit = limit.unwrap_or(20).min(200);

    let walk = repo
        .rev_walk([tip])
        .sorting(gix::revision::walk::Sorting::ByCommitTime(
            gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
        ))
        .all()
        .map_err(|e| format!("rev-walk failed: {e}"))?;

    let mut lines = Vec::new();
    for info in walk.take(limit) {
        let info = info.map_err(|e| e.to_string())?;
        let oid = info.id;
        let obj = repo.find_object(oid).map_err(|e| e.to_string())?;
        let commit = obj.peel_to_commit().map_err(|e| e.to_string())?;
        let author = commit.author().map_err(|e| e.to_string())?;
        let msg = commit.message().map_err(|e| e.to_string())?;

        lines.push(format!(
            "{} {} <{}> {} {}",
            &oid.to_string()[..12],
            author.name.to_str_lossy(),
            author.email.to_str_lossy(),
            fmt_time(author.time.seconds),
            msg.title.to_str_lossy(),
        ));
    }

    if lines.is_empty() {
        Ok("(no commits found)".into())
    } else {
        Ok(lines.join("\n"))
    }
}

fn cmd_show(
    repo: &gix::Repository,
    revision: &str,
    include_diff: bool,
    max_lines: usize,
) -> Result<String, String> {
    let oid = resolve_rev(repo, revision)?;
    let obj = repo.find_object(oid).map_err(|e| e.to_string())?;
    let commit = obj.peel_to_commit().map_err(|e| e.to_string())?;

    let author = commit.author().map_err(|e| e.to_string())?;
    let committer = commit.committer().map_err(|e| e.to_string())?;
    let msg = commit.message().map_err(|e| e.to_string())?;
    let full_body = msg
        .body
        .map(|b| b.to_str_lossy().into_owned())
        .unwrap_or_default();

    let mut out = format!(
        "commit {oid}\nAuthor:    {} <{}>\nCommitter: {} <{}>\nDate:      {}\n\n    {}\n",
        author.name.to_str_lossy(),
        author.email.to_str_lossy(),
        committer.name.to_str_lossy(),
        committer.email.to_str_lossy(),
        fmt_time(committer.time.seconds),
        msg.title.to_str_lossy(),
    );
    for line in full_body.lines() {
        out.push_str(&format!("    {line}\n"));
    }

    // Obtain the new tree and the parent tree (None for the initial commit).
    let new_tree = commit.tree().map_err(|e| e.to_string())?;

    let parent_ids: Vec<_> = commit.parent_ids().collect();
    // `diff_tree_to_tree` with None as old_tree treats everything as additions.
    let parent_tree: Option<gix::Tree<'_>> = if parent_ids.is_empty() {
        None
    } else {
        let parent_oid = parent_ids[0].detach();
        Some(
            repo.find_object(parent_oid)
                .map_err(|e| e.to_string())?
                .peel_to_commit()
                .map_err(|e| e.to_string())?
                .tree()
                .map_err(|e| e.to_string())?,
        )
    };

    // Build the changed-file summary using gix's diff engine (includes renames).
    let opts = gix::diff::Options::default().with_rewrites(Some(gix::diff::Rewrites::default()));
    let changes = repo
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&new_tree), opts)
        .map_err(|e| e.to_string())?;

    use gix::object::tree::diff::ChangeDetached;
    let mut changed: Vec<String> = Vec::new();
    for change in &changes {
        match change {
            ChangeDetached::Addition { location, .. } => {
                changed.push(format!("A  {}", location.to_str_lossy()))
            }
            ChangeDetached::Deletion { location, .. } => {
                changed.push(format!("D  {}", location.to_str_lossy()))
            }
            ChangeDetached::Modification {
                location,
                previous_entry_mode,
                entry_mode,
                ..
            } => {
                if previous_entry_mode != entry_mode {
                    changed.push(format!(
                        "M  {} (mode {} → {})",
                        location.to_str_lossy(),
                        previous_entry_mode.kind().as_octal_str(),
                        entry_mode.kind().as_octal_str()
                    ));
                } else {
                    changed.push(format!("M  {}", location.to_str_lossy()));
                }
            }
            ChangeDetached::Rewrite {
                source_location,
                location,
                copy,
                ..
            } => {
                let verb = if *copy { "C" } else { "R" };
                changed.push(format!(
                    "{verb}  {} → {}",
                    source_location.to_str_lossy(),
                    location.to_str_lossy()
                ));
            }
        }
    }

    if changed.is_empty() {
        out.push_str("\n(no file changes)\n");
    } else {
        out.push_str(&format!("\n{} file(s) changed:\n", changed.len()));
        for f in &changed {
            out.push_str(&format!("  {f}\n"));
        }
    }

    if include_diff {
        out.push('\n');
        out.push_str(&diff_trees(
            repo,
            parent_tree.as_ref(),
            Some(&new_tree),
            max_lines,
        )?);
    }

    Ok(out)
}

fn cmd_diff_commits(
    repo: &gix::Repository,
    old_rev: &str,
    new_rev: &str,
    max_lines: usize,
) -> Result<String, String> {
    let old_oid = resolve_rev(repo, old_rev)?;
    let new_oid = resolve_rev(repo, new_rev)?;

    let old_tree = repo
        .find_object(old_oid)
        .map_err(|e| e.to_string())?
        .peel_to_commit()
        .map_err(|e| e.to_string())?
        .tree()
        .map_err(|e| e.to_string())?;

    let new_tree = repo
        .find_object(new_oid)
        .map_err(|e| e.to_string())?
        .peel_to_commit()
        .map_err(|e| e.to_string())?
        .tree()
        .map_err(|e| e.to_string())?;

    diff_trees(repo, Some(&old_tree), Some(&new_tree), max_lines)
}

fn cmd_diff_worktree(
    repo: &gix::Repository,
    summary_only: bool,
    max_lines: usize,
) -> Result<String, String> {
    use gix::bstr::ByteSlice as _;
    use std::collections::HashSet;

    let work_dir = repo
        .work_dir()
        .ok_or("bare repositories have no working tree")?
        .to_path_buf();

    let head_oid = repo
        .head_id()
        .map_err(|e| format!("cannot resolve HEAD: {e}"))?
        .detach();
    let head_blobs = match repo
        .find_object(head_oid)
        .map_err(|e| e.to_string())
        .and_then(|object| object.peel_to_commit().map_err(|e| e.to_string()))
        .and_then(|commit| commit.tree_id().map_err(|e| e.to_string()))
    {
        Ok(tree_id) => tree_blobs(repo, tree_id.detach())?,
        Err(err) => {
            // Some repositories can have a resolvable HEAD reference while objects needed to peel
            // it are unavailable locally. Keep diff_worktree best-effort instead of failing.
            return Ok(format!(
                "(working tree diff unavailable: cannot inspect HEAD tree: {err})"
            ));
        }
    };

    // Build a set of HEAD paths for fast lookup in pass 1.5.
    let head_path_set: HashSet<gix::bstr::BString> =
        head_blobs.iter().map(|(p, _)| p.clone()).collect();

    // Load index once — pass 1.5 borrows it, then it is moved into dirwalk_iter.
    let index = repo.index_or_empty().map_err(|e| e.to_string())?;

    let mut output = String::new();
    let mut total_lines = 0usize;
    let truncation_msg = format!(
        "[diff truncated at {max_lines} lines — use summary_only=true for file list only]\n"
    );

    // ── 1. Modified / deleted tracked files (HEAD tree vs disk) ──────────────
    for (path_bytes, head_oid) in &head_blobs {
        let rel = path_bytes.to_str_lossy();
        let disk_path = work_dir.join(rel.as_ref());

        if !disk_path.exists() {
            output.push_str(&format!("D  {rel}\n"));
            continue;
        }

        let disk_content =
            std::fs::read(&disk_path).map_err(|e| format!("cannot read {rel}: {e}"))?;
        let head_content = match repo.find_object(*head_oid) {
            Ok(object) => object.data.to_vec(),
            Err(err) => {
                // The worktree may momentarily reference objects unavailable in the local
                // object database (for example in partial/shallow setups or unusual local
                // repository states). Treat such entries as unreadable instead of failing the
                // whole command so diff_worktree remains best-effort and non-panicking.
                output.push_str(&format!("!  {rel}  [cannot read HEAD blob: {err}]\n"));
                continue;
            }
        };

        if disk_content != head_content {
            if summary_only {
                output.push_str(&format!("M  {rel}\n"));
            } else {
                let patch = unified_diff_modify(&rel, &head_content, &disk_content);
                total_lines += patch.lines().count();
                output.push_str(&patch);
                if total_lines > max_lines {
                    output.push_str(&truncation_msg);
                    return Ok(output);
                }
            }
        }
    }

    // ── 1.5. Newly staged files (in index, absent from HEAD) ─────────────────
    // Files that were `git add`ed but never committed land in the index without
    // appearing in the HEAD tree.  Pass 1 only iterates head_blobs so it misses
    // them; dirwalk_iter (pass 2) won't show them as Untracked because they ARE
    // tracked in the index.  We detect them by walking the index and filtering
    // out paths that are already present in HEAD.
    {
        let path_backing = index.path_backing();
        for entry in index.entries() {
            // Stage 0 = normal entry; stages 1/2/3 are merge-conflict markers.
            if entry.flags.stage() != gix::index::entry::Stage::Unconflicted {
                continue;
            }
            let path = entry.path_in(&path_backing);
            let path_owned: gix::bstr::BString = path.to_owned();
            if head_path_set.contains(&path_owned) {
                continue; // already covered by pass 1
            }
            let rel = path.to_str_lossy();
            if summary_only {
                output.push_str(&format!("A  {rel}\n"));
            } else {
                let blob_data = match repo.find_object(entry.id) {
                    Ok(object) => object.data.to_vec(),
                    Err(err) => {
                        output.push_str(&format!("!  {rel}  [cannot read index blob: {err}]\n"));
                        continue;
                    }
                };
                let patch = unified_diff_add(&rel, &blob_data);
                total_lines += patch.lines().count();
                output.push_str(&patch);
                if total_lines > max_lines {
                    output.push_str(&truncation_msg);
                    return Ok(output);
                }
            }
        }
    } // path_backing borrow released; index can now be moved into dirwalk_iter

    // ── 2. Untracked files (gitignore-aware directory walk) ───────────────────
    // `dirwalk_iter` uses the git index + .gitignore rules to classify each
    // filesystem entry.  Only entries whose status is `Untracked` (i.e. not in
    // the index and not gitignored) are reported here.
    let options = repo
        .dirwalk_options()
        .map_err(|e| e.to_string())?
        // Emit individual untracked files (not collapsed into directories).
        .emit_untracked(gix::dir::walk::EmissionMode::Matching)
        // Do NOT emit gitignored entries — git status doesn't show them by default.
        .emit_ignored(None);

    let interrupt = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let iter = repo
        .dirwalk_iter(index, std::iter::empty::<&str>(), interrupt.into(), options)
        .map_err(|e| e.to_string())?;

    for item in iter {
        let item = item.map_err(|e| e.to_string())?;
        // We only care about plain untracked files (not symlinks, repos, etc.).
        if item.entry.status != gix::dir::entry::Status::Untracked {
            continue;
        }
        if !matches!(item.entry.disk_kind, Some(gix::dir::entry::Kind::File)) {
            continue;
        }

        let rel = item.entry.rela_path.to_str_lossy();
        if summary_only {
            output.push_str(&format!("?  {rel}\n"));
        } else {
            let disk_path = work_dir.join(rel.as_ref());
            let data = std::fs::read(&disk_path).map_err(|e| format!("cannot read {rel}: {e}"))?;
            let patch = unified_diff_add(&rel, &data);
            total_lines += patch.lines().count();
            output.push_str(&patch);
            if total_lines > max_lines {
                output.push_str(&truncation_msg);
                return Ok(output);
            }
        }
    }

    if output.is_empty() {
        Ok(
            "(working tree is clean — no modifications to tracked files and no untracked files)"
                .into(),
        )
    } else {
        Ok(output)
    }
}

fn cmd_read_file(repo: &gix::Repository, revision: &str, path: &str) -> Result<String, String> {
    let oid = resolve_rev(repo, revision)?;
    let tree_id = repo
        .find_object(oid)
        .map_err(|e| e.to_string())?
        .peel_to_commit()
        .map_err(|e| e.to_string())?
        .tree_id()
        .map_err(|e| e.to_string())?
        .detach();

    let blobs = tree_blobs(repo, tree_id)?;
    let target = path.trim_start_matches("./").trim_start_matches('/');

    let blob_oid = blobs
        .iter()
        .find(|(p, _)| p.as_slice() == target.as_bytes())
        .map(|(_, o)| *o)
        .ok_or_else(|| format!("file '{path}' not found in tree at revision '{revision}'"))?;

    let data = repo
        .find_object(blob_oid)
        .map_err(|e| e.to_string())?
        .data
        .to_vec();

    if is_binary(&data) {
        return Err(format!("'{path}' is a binary file"));
    }

    String::from_utf8(data).map_err(|_| format!("'{path}' contains non-UTF-8 bytes"))
}

fn cmd_status(repo: &gix::Repository) -> Result<String, String> {
    use gix::bstr::ByteSlice as _;
    use std::collections::HashSet;

    let work_dir = repo
        .work_dir()
        .ok_or("bare repositories have no working tree")?
        .to_path_buf();

    let mut output = String::new();

    // Current branch
    let head_ref = repo.head_ref().ok().flatten();
    let branch = head_ref
        .as_ref()
        .map(|r| {
            r.name()
                .as_bstr()
                .to_str_lossy()
                .strip_prefix("refs/heads/")
                .unwrap_or(&r.name().as_bstr().to_str_lossy())
                .to_string()
        })
        .unwrap_or_else(|| {
            repo.head_id()
                .map(|id| format!("(detached at {})", &id.to_string()[..8]))
                .unwrap_or_else(|_| "(no commits)".into())
        });
    output.push_str(&format!("On branch {branch}\n\n"));

    // Get HEAD tree blobs
    let head_blobs = match repo.head_id() {
        Ok(oid) => {
            let tree_id = repo
                .find_object(oid.detach())
                .map_err(|e| e.to_string())?
                .peel_to_commit()
                .map_err(|e| e.to_string())?
                .tree_id()
                .map_err(|e| e.to_string())?
                .detach();
            tree_blobs(repo, tree_id)?
        }
        Err(_) => Vec::new(), // empty repo
    };

    let head_path_set: HashSet<gix::bstr::BString> =
        head_blobs.iter().map(|(p, _)| p.clone()).collect();

    let index = repo.index_or_empty().map_err(|e| e.to_string())?;

    // Staged new files (in index but not in HEAD)
    let mut staged: Vec<String> = Vec::new();
    for entry in index.entries() {
        let path = entry.path(&index);
        if !head_path_set.contains(path) {
            staged.push(path.to_str_lossy().to_string());
        }
    }

    // Modified and deleted tracked files (HEAD vs working tree)
    let mut modified: Vec<String> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    for (path_bytes, head_oid) in &head_blobs {
        let rel = path_bytes.to_str_lossy();
        let disk_path = work_dir.join(rel.as_ref());

        if !disk_path.exists() {
            deleted.push(rel.to_string());
            continue;
        }

        let disk_content =
            std::fs::read(&disk_path).map_err(|e| format!("cannot read {rel}: {e}"))?;
        let head_content = repo
            .find_object(*head_oid)
            .map_err(|e| e.to_string())?
            .data
            .to_vec();

        if disk_content != head_content {
            modified.push(rel.to_string());
        }
    }

    // Untracked files (via dirwalk)
    let mut untracked: Vec<String> = Vec::new();
    let index_for_walk = repo.index_or_empty().map_err(|e| e.to_string())?;
    let dir_walk = repo
        .dirwalk_iter(
            index_for_walk,
            Vec::<gix::bstr::BString>::new(),
            Default::default(),
            repo.dirwalk_options()
                .map_err(|e| format!("dirwalk options: {e}"))?,
        )
        .map_err(|e| format!("dirwalk: {e}"))?;

    for item in dir_walk {
        match item {
            Ok(entry) => {
                if entry.entry.status == gix::dir::entry::Status::Untracked {
                    let p = entry.entry.rela_path.to_str_lossy().to_string();
                    untracked.push(p);
                }
            }
            Err(_) => continue,
        }
    }

    // Format output
    if !staged.is_empty() {
        output.push_str("Staged (new files):\n");
        for f in &staged {
            output.push_str(&format!("  A  {f}\n"));
        }
        output.push('\n');
    }

    if !modified.is_empty() {
        output.push_str("Modified:\n");
        for f in &modified {
            output.push_str(&format!("  M  {f}\n"));
        }
        output.push('\n');
    }

    if !deleted.is_empty() {
        output.push_str("Deleted:\n");
        for f in &deleted {
            output.push_str(&format!("  D  {f}\n"));
        }
        output.push('\n');
    }

    if !untracked.is_empty() {
        output.push_str("Untracked:\n");
        for f in &untracked {
            output.push_str(&format!("  ?  {f}\n"));
        }
        output.push('\n');
    }

    if staged.is_empty() && modified.is_empty() && deleted.is_empty() && untracked.is_empty() {
        output.push_str("Working tree clean.\n");
    }

    Ok(output)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::AllowAll;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext {
            working_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            session_id: "test-git".into(),
            permissions: Arc::new(AllowAll),
            cost_tracker: Arc::new(crate::CostTracker::new()),
            mcp_manager: None,
            extensions: crate::Extensions::default(),
            network_policy: None,
        }
    }

    #[tokio::test]
    async fn log_runs() {
        let t = GitTool;
        let r = t
            .execute(
                serde_json::json!({"command": "log", "limit": 5}),
                &test_ctx(),
            )
            .await;
        assert!(!r.is_error, "log failed: {}", r.content);
        assert!(!r.content.is_empty());
    }

    #[tokio::test]
    async fn show_head() {
        let t = GitTool;
        let r = t
            .execute(
                serde_json::json!({"command": "show", "revision": "HEAD"}),
                &test_ctx(),
            )
            .await;
        assert!(!r.is_error, "show failed: {}", r.content);
        assert!(r.content.contains("commit "));
    }

    #[tokio::test]
    async fn show_head_with_custom_max_diff_lines() {
        let t = GitTool;
        let r = t.execute(
            serde_json::json!({"command": "show", "revision": "HEAD", "include_diff": true, "max_diff_lines": 50}),
            &test_ctx(),
        ).await;
        assert!(!r.is_error, "show+diff failed: {}", r.content);
    }

    #[tokio::test]
    async fn diff_worktree_does_not_panic() {
        let t = GitTool;
        let r = t
            .execute(serde_json::json!({"command": "diff_worktree"}), &test_ctx())
            .await;
        assert!(!r.is_error, "diff_worktree failed: {}", r.content);
    }

    #[tokio::test]
    async fn diff_commits_head_vs_parent() {
        let t = GitTool;
        let r = t.execute(
            serde_json::json!({"command": "diff_commits", "old_rev": "HEAD~1", "new_rev": "HEAD"}),
            &test_ctx(),
        ).await;
        // HEAD~1 may not exist for a single-commit repo; either outcome is fine.
        let _ = r;
    }

    #[tokio::test]
    async fn unknown_command_returns_error() {
        let t = GitTool;
        let r = t
            .execute(serde_json::json!({"command": "nuke"}), &test_ctx())
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("unknown command"));
    }

    // ── Isolated-repo helpers ────────────────────────────────────────────────

    use std::process::Command;

    fn init_test_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        let ok = |mut c: Command| {
            let st = c.current_dir(p).status().expect("spawn git");
            assert!(st.success(), "git command failed");
        };
        let mut init = Command::new("git");
        init.args(["init"]);
        ok(init);
        let mut cfg_email = Command::new("git");
        cfg_email.args(["config", "user.email", "test@example.com"]);
        ok(cfg_email);
        let mut cfg_name = Command::new("git");
        cfg_name.args(["config", "user.name", "Test"]);
        ok(cfg_name);
        dir
    }

    fn commit_file(repo: &std::path::Path, filename: &str, content: &[u8], msg: &str) {
        std::fs::write(repo.join(filename), content).expect("write");
        let ok = |mut c: Command| {
            let st = c.current_dir(repo).status().expect("spawn git");
            assert!(st.success(), "git command failed");
        };
        let mut add = Command::new("git");
        add.args(["add", filename]);
        ok(add);
        let mut commit = Command::new("git");
        commit.args(["commit", "-m", msg]);
        ok(commit);
    }

    fn commit_file_with_dates(
        repo: &std::path::Path,
        filename: &str,
        content: &[u8],
        msg: &str,
        author_date: &str,
        committer_date: &str,
    ) {
        std::fs::write(repo.join(filename), content).expect("write");
        let st = Command::new("git")
            .args(["add", filename])
            .current_dir(repo)
            .status()
            .expect("spawn git add");
        assert!(st.success(), "git add failed");

        let st = Command::new("git")
            .args(["commit", "-m", msg])
            .env("GIT_AUTHOR_DATE", author_date)
            .env("GIT_COMMITTER_DATE", committer_date)
            .current_dir(repo)
            .status()
            .expect("spawn git commit");
        assert!(st.success(), "git commit failed");
    }

    fn ctx_for(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            session_id: "test-iso".into(),
            permissions: Arc::new(AllowAll),
            cost_tracker: Arc::new(crate::CostTracker::new()),
            mcp_manager: None,
            extensions: crate::Extensions::default(),
            network_policy: None,
        }
    }

    // ── Correctness tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn read_file_returns_exact_content() {
        let dir = init_test_repo();
        commit_file(dir.path(), "hello.txt", b"hello world\n", "add hello");
        let t = GitTool;
        let r = t
            .execute(
                serde_json::json!({"command": "read_file", "path": "hello.txt"}),
                &ctx_for(dir.path()),
            )
            .await;
        assert!(!r.is_error, "read_file failed: {}", r.content);
        assert_eq!(r.content.trim(), "hello world");
    }

    #[tokio::test]
    async fn read_file_binary_returns_error() {
        let dir = init_test_repo();
        // Write a file with null bytes — detected as binary
        let mut data = b"binary\x00data".to_vec();
        data.extend_from_slice(&[0u8; 20]);
        commit_file(dir.path(), "bin.bin", &data, "add binary");
        let t = GitTool;
        let r = t
            .execute(
                serde_json::json!({"command": "read_file", "path": "bin.bin"}),
                &ctx_for(dir.path()),
            )
            .await;
        assert!(r.is_error, "expected error for binary, got: {}", r.content);
        assert!(
            r.content.to_lowercase().contains("binary"),
            "error should mention 'binary': {}",
            r.content
        );
    }

    #[tokio::test]
    async fn show_uses_committer_timestamp_for_date() {
        let dir = init_test_repo();
        commit_file_with_dates(
            dir.path(),
            "dated.txt",
            b"dated\n",
            "dated commit",
            "2001-02-03T04:05:06Z",
            "2022-11-12T13:14:15Z",
        );
        let t = GitTool;
        let r = t
            .execute(
                serde_json::json!({"command": "show", "revision": "HEAD"}),
                &ctx_for(dir.path()),
            )
            .await;
        assert!(!r.is_error, "show failed: {}", r.content);
        assert!(
            r.content.contains("Date:      2022-11-12 13:14:15 UTC"),
            "expected show output to use committer timestamp, got:\n{}",
            r.content
        );
        assert!(
            !r.content.contains("Date:      2001-02-03 04:05:06 UTC"),
            "show output incorrectly used author timestamp:\n{}",
            r.content
        );
    }

    #[tokio::test]
    async fn diff_worktree_detects_modification() {
        let dir = init_test_repo();
        commit_file(dir.path(), "file.txt", b"original\n", "initial");
        // Modify the tracked file without staging
        std::fs::write(dir.path().join("file.txt"), b"modified\n").expect("write");
        let t = GitTool;
        let r = t
            .execute(
                serde_json::json!({"command": "diff_worktree", "summary_only": true}),
                &ctx_for(dir.path()),
            )
            .await;
        assert!(!r.is_error, "diff_worktree failed: {}", r.content);
        assert!(
            r.content.contains("file.txt"),
            "expected file.txt in diff_worktree output, got:\n{}",
            r.content
        );
        assert!(
            r.content.contains('M'),
            "expected 'M' status marker, got:\n{}",
            r.content
        );
    }

    #[tokio::test]
    async fn diff_worktree_detects_staged_new_file() {
        let dir = init_test_repo();
        commit_file(dir.path(), "existing.txt", b"exists\n", "initial");
        // Create a new file and stage it — disk matches index, both absent from HEAD.
        std::fs::write(dir.path().join("staged_new.txt"), b"brand new staged\n").expect("write");
        let st = Command::new("git")
            .args(["add", "staged_new.txt"])
            .current_dir(dir.path())
            .status()
            .expect("git add");
        assert!(st.success());
        let t = GitTool;
        let r = t
            .execute(
                serde_json::json!({"command": "diff_worktree", "summary_only": true}),
                &ctx_for(dir.path()),
            )
            .await;
        assert!(!r.is_error, "diff_worktree failed: {}", r.content);
        assert!(
            r.content.contains("staged_new.txt"),
            "expected staged_new.txt in output, got:\n{}",
            r.content
        );
        assert!(
            r.content.contains('A'),
            "expected 'A' status marker for staged new file, got:\n{}",
            r.content
        );
    }

    #[tokio::test]
    async fn diff_worktree_detects_untracked() {
        let dir = init_test_repo();
        commit_file(dir.path(), "existing.txt", b"exists\n", "initial");
        // Create an untracked file
        std::fs::write(dir.path().join("new_file.txt"), b"brand new\n").expect("write");
        let t = GitTool;
        let r = t
            .execute(
                serde_json::json!({"command": "diff_worktree", "summary_only": true}),
                &ctx_for(dir.path()),
            )
            .await;
        assert!(!r.is_error, "diff_worktree failed: {}", r.content);
        assert!(
            r.content.contains("new_file.txt"),
            "expected new_file.txt listed as untracked, got:\n{}",
            r.content
        );
        assert!(
            r.content.contains('?'),
            "expected '?' status marker for untracked, got:\n{}",
            r.content
        );
    }

    #[tokio::test]
    async fn diff_worktree_detects_deletion() {
        let dir = init_test_repo();
        commit_file(dir.path(), "to_delete.txt", b"goodbye\n", "add file");
        // Delete the tracked file from disk
        std::fs::remove_file(dir.path().join("to_delete.txt")).expect("remove");
        let t = GitTool;
        let r = t
            .execute(
                serde_json::json!({"command": "diff_worktree", "summary_only": true}),
                &ctx_for(dir.path()),
            )
            .await;
        assert!(!r.is_error, "diff_worktree failed: {}", r.content);
        assert!(
            r.content.contains("to_delete.txt"),
            "expected to_delete.txt listed as deleted, got:\n{}",
            r.content
        );
        assert!(
            r.content.contains('D'),
            "expected 'D' status marker, got:\n{}",
            r.content
        );
    }

    #[tokio::test]
    async fn diff_commits_output_shape() {
        let dir = init_test_repo();
        commit_file(dir.path(), "alpha.txt", b"line1\nline2\n", "first");
        commit_file(dir.path(), "alpha.txt", b"line1\nline2\nline3\n", "second");
        let t = GitTool;
        let r = t.execute(
            serde_json::json!({"command": "diff_commits", "old_rev": "HEAD~1", "new_rev": "HEAD"}),
            &ctx_for(dir.path()),
        ).await;
        assert!(!r.is_error, "diff_commits failed: {}", r.content);
        assert!(
            r.content.contains("diff --git"),
            "expected unified diff header, got:\n{}",
            r.content
        );
        assert!(
            r.content.contains("alpha.txt"),
            "expected filename in diff, got:\n{}",
            r.content
        );
        assert!(
            r.content.contains("+line3"),
            "expected added line in diff, got:\n{}",
            r.content
        );
    }

    #[tokio::test]
    async fn show_truncates_at_max_diff_lines() {
        let dir = init_test_repo();
        // Create a file large enough to exceed a tiny max_diff_lines limit
        let content: String = (0..200).map(|i| format!("line {i}\n")).collect();
        commit_file(dir.path(), "big.txt", content.as_bytes(), "big commit");
        let t = GitTool;
        let r = t
            .execute(
                serde_json::json!({
                    "command": "show",
                    "revision": "HEAD",
                    "include_diff": true,
                    "max_diff_lines": 10
                }),
                &ctx_for(dir.path()),
            )
            .await;
        assert!(!r.is_error, "show failed: {}", r.content);
        assert!(
            r.content.contains("truncated") || r.content.contains("…"),
            "expected truncation notice, got:\n{}",
            r.content
        );
    }
}
