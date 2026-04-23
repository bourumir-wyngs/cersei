//! Grep tool: session-scoped tagged search backed by XFileStorage.

use super::*;
use crate::xfile_storage::{
    resolve_xfile_path, store_loaded_if_missing, try_get_head, xfile_session_id, XFile, XLine,
};
use regex::RegexBuilder;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

static GREP_COUNTER_REGISTRY: once_cell::sync::Lazy<dashmap::DashMap<String, usize>> =
    once_cell::sync::Lazy::new(dashmap::DashMap::new);

pub fn clear_grep_counters(session_id: &str) {
    GREP_COUNTER_REGISTRY.remove(session_id);
}

pub struct XGrepTool;

/// Public alias preserved for downstream imports.
pub type FileXGrepTool = XGrepTool;

struct SearchFileResult {
    file: XFile,
    match_indices: Vec<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct XGrepRequest {
    pub pattern: String,
    pub path: String,
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    pub case_sensitive: Option<bool>,
    #[serde(default)]
    pub before: Option<usize>,
    #[serde(default)]
    pub after: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Internal flag to suppress the MultiGrep nudge when called from MultiGrep.
    #[serde(default)]
    pub suppress_nudge: Option<bool>,
}

#[async_trait]
impl Tool for XGrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Search a file or directory using a regular expression against the latest session-scoped XFileStorage revision of each file. If a searched file is not already in XFileStorage, Grep reads it from disk first. Only files that produce at least one match are added to XFileStorage. Output lines are returned as `<path>:<tag>:<content>`, where `tag` is the stable unique line identifier to use with Edit or Read. Optional `before` and `after` parameters include surrounding context lines around each match. `limit` defaults to 32 total matches."
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
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern in Rust `regex` crate syntax, matched against each line separately. Supports groups, alternation, character classes, anchors, and quantifiers. Does not support backreferences or look-around."
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search in. Absolute paths and workspace-relative paths are accepted."
                },
                "glob": {
                    "type": "string",
                    "description": "Optional glob filter for candidate files, for example `*.rs`."
                },
                "case_sensitive": {
                    "type": "boolean",
                    "description": "Optional case-sensitivity override. Defaults to true."
                },
                "before": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Optional number of context lines to include before each match."
                },
                "after": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Optional number of context lines to include after each match."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum total number of matches to return across all searched files. Context lines from `before` and `after` do not count toward this limit. Defaults to 32."
                }
            },
            "required": ["pattern", "path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let req: XGrepRequest = match serde_json::from_value(input) {
            Ok(req) => req,
            Err(err) => return ToolResult::error(format!("Invalid input: {}", err)),
        };

        let path = resolve_xfile_path(ctx, &req.path);
        if !path.exists() {
            return ToolResult::error(format!("Path not found: {}", path.display()));
        }

        let regex = match RegexBuilder::new(&req.pattern)
            .case_insensitive(!req.case_sensitive.unwrap_or(true))
            .build()
        {
            Ok(regex) => regex,
            Err(err) => return ToolResult::error(format!("Invalid regex: {}", err)),
        };

        let limit = req.limit.unwrap_or(32);
        let before = req.before.unwrap_or(0);
        let after = req.after.unwrap_or(0);
        let glob = match req.glob.as_deref() {
            Some(pattern) => match glob::Pattern::new(pattern) {
                Ok(pattern) => Some(pattern),
                Err(err) => return ToolResult::error(format!("Invalid glob: {}", err)),
            },
            None => None,
        };

        let candidates = collect_candidate_files(&path, glob.as_ref());
        let storage_session_id = xfile_session_id(ctx);
        let mut hits = Vec::new();
        let mut match_count = 0usize;
        let mut truncated = false;

        for candidate in candidates {
            let searched = match search_file(candidate.as_path(), &storage_session_id, &regex).await
            {
                Ok(result) => result,
                Err(err) => return ToolResult::error(err),
            };
            let Some(searched) = searched else {
                continue;
            };

            let remaining = limit.saturating_sub(match_count);
            if searched.match_indices.len() > remaining {
                hits.extend(
                    select_context_lines(
                        &searched.file,
                        &searched.match_indices[..remaining],
                        before,
                        after,
                    )
                    .into_iter()
                    .map(|line| format_output_line(&searched.file.path, line)),
                );
                truncated = true;
                break;
            }

            match_count += searched.match_indices.len();
            hits.extend(
                select_context_lines(&searched.file, &searched.match_indices, before, after)
                    .into_iter()
                    .map(|line| format_output_line(&searched.file.path, line)),
            );
        }

        let mut content = if hits.is_empty() {
            "No matches found.".to_string()
        } else if truncated {
            format!(
                "{}\n\n[more matches found, capped to {}]",
                hits.join("\n"),
                limit
            )
        } else {
            hits.join("\n")
        };

        if !req.suppress_nudge.unwrap_or(false) {
            let grep_count = {
                let mut entry = GREP_COUNTER_REGISTRY
                    .entry(ctx.session_id.clone())
                    .or_insert(0);
                *entry += 1;
                *entry
            };

            if grep_count >= 3 && grep_count % 3 == 0 {
                content.push_str("\n\nNOTE: You have called Grep multiple times in this session. For better efficiency, consider using MultiGrep to perform multiple searches in a single request.");
            }
        }

        ToolResult::success(content)
    }
}

fn collect_candidate_files(root: &Path, glob: Option<&glob::Pattern>) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if root.is_file() {
        if matches_glob(root, root.parent().unwrap_or(root), glob) {
            files.push(root.to_path_buf());
        }
        return files;
    }

    for entry in WalkDir::new(root)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        let path = entry.path();
        if path.is_file() && matches_glob(path, root, glob) {
            files.push(path.to_path_buf());
        }
    }

    files.sort();
    files
}

fn matches_glob(path: &Path, root: &Path, glob: Option<&glob::Pattern>) -> bool {
    let Some(glob) = glob else {
        return true;
    };

    if glob.matches_path(path) {
        return true;
    }

    if let Ok(relative) = path.strip_prefix(root) {
        if glob.matches_path(relative) {
            return true;
        }
    }

    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| glob.matches(name))
        .unwrap_or(false)
}

async fn search_file(
    path: &Path,
    session_id: &str,
    regex: &regex::Regex,
) -> std::result::Result<Option<SearchFileResult>, String> {
    if let Some(head) = try_get_head(session_id, path) {
        let match_indices = matching_line_indices(&head.file.content, regex);
        return if match_indices.is_empty() {
            Ok(None)
        } else {
            Ok(Some(SearchFileResult {
                file: head.file,
                match_indices,
            }))
        };
    }

    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("Failed to read file: {}", e))?;
    if !text.lines().any(|line| regex.is_match(line)) {
        return Ok(None);
    }

    let head = store_loaded_if_missing(session_id, path, &text);
    let match_indices = matching_line_indices(&head.file.content, regex);
    Ok(Some(SearchFileResult {
        file: head.file,
        match_indices,
    }))
}

fn matching_line_indices(lines: &[XLine], regex: &regex::Regex) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| regex.is_match(&line.content).then_some(idx))
        .collect()
}

fn select_context_lines<'a>(
    file: &'a XFile,
    match_indices: &[usize],
    before: usize,
    after: usize,
) -> Vec<&'a XLine> {
    if match_indices.is_empty() {
        return Vec::new();
    }

    let last_idx = file.content.len().saturating_sub(1);
    let mut windows: Vec<(usize, usize)> = Vec::new();

    for &match_idx in match_indices {
        let start = match_idx.saturating_sub(before);
        let end = match_idx.saturating_add(after).min(last_idx);

        if let Some((_, last_end)) = windows.last_mut() {
            if start <= last_end.saturating_add(1) {
                *last_end = (*last_end).max(end);
                continue;
            }
        }

        windows.push((start, end));
    }

    let mut lines = Vec::new();
    for (start, end) in windows {
        lines.extend(file.content[start..=end].iter());
    }
    lines
}

fn format_output_line(path: &Path, line: &XLine) -> String {
    format!("{}:{}:{}", path.display(), line.tag, line.content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::AllowAll;
    use crate::xfile_storage::{clear_session_xfile_storage, store_written_text, try_get_head};
    use std::sync::Arc;

    fn test_ctx(root: &Path, session_id: &str) -> ToolContext {
        ToolContext {
            working_dir: root.to_path_buf(),
            session_id: session_id.into(),
            permissions: Arc::new(AllowAll),
            cost_tracker: Arc::new(CostTracker::new()),
            mcp_manager: None,
            extensions: Extensions::default(),
            network_policy: None,
        }
    }

    #[test]
    fn xgrep_schema_exposes_search_inputs() {
        let tool = XGrepTool;
        let schema = tool.input_schema();

        assert_eq!(schema["properties"]["pattern"]["type"], "string");
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(schema["properties"]["before"]["minimum"], 0);
        assert_eq!(schema["properties"]["after"]["minimum"], 0);
        assert_eq!(schema["properties"]["limit"]["minimum"], 1);
    }

    #[test]
    fn filesystem_toolset_includes_xgrep() {
        let names: Vec<String> = crate::filesystem()
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect();

        assert!(names.iter().any(|name| name == "Grep"));
    }

    #[tokio::test]
    async fn xgrep_uses_tags_and_only_loads_matching_files() {
        let tmp = tempfile::tempdir().unwrap();
        clear_session_xfile_storage("xgrep-match-test");
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        tokio::fs::write(&a, "todo one\nkeep\n").await.unwrap();
        tokio::fs::write(&b, "no match here\n").await.unwrap();

        let tool = XGrepTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "pattern": "todo",
                    "path": tmp.path().display().to_string()
                }),
                &test_ctx(tmp.path(), "xgrep-match-test"),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("a.txt:"));
        assert!(result.content.contains(":todo one"));
        assert_eq!(try_get_head("xgrep-match-test", &a).is_some(), true);
        assert_eq!(try_get_head("xgrep-match-test", &b).is_some(), false);
    }

    #[tokio::test]
    async fn xgrep_reads_latest_storage_revision() {
        let tmp = tempfile::tempdir().unwrap();
        clear_session_xfile_storage("xgrep-storage-test");
        let path = tmp.path().join("tracked.txt");
        let head = store_written_text("xgrep-storage-test", &path, "fresh value\nold value\n");
        tokio::fs::write(&path, "stale disk copy\n").await.unwrap();

        let tool = XGrepTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "pattern": "fresh",
                    "path": path.display().to_string()
                }),
                &test_ctx(tmp.path(), "xgrep-storage-test"),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains(&head.file.content[0].tag));
        assert!(result.content.contains("fresh value"));
        assert!(!result.content.contains("stale disk copy"));
    }

    #[tokio::test]
    async fn xgrep_rejects_invalid_regex() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "xgrep-invalid-regex";
        clear_session_xfile_storage(session_id);
        let path = tmp.path().join("sample.txt");
        tokio::fs::write(&path, "alpha\n").await.unwrap();

        let tool = XGrepTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "pattern": "(",
                    "path": path.display().to_string()
                }),
                &test_ctx(tmp.path(), session_id),
            )
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("Invalid regex"));
    }

    #[tokio::test]
    async fn xgrep_rejects_invalid_glob() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "xgrep-invalid-glob";
        clear_session_xfile_storage(session_id);
        let path = tmp.path().join("sample.txt");
        tokio::fs::write(&path, "alpha\n").await.unwrap();

        let tool = XGrepTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "pattern": "alpha",
                    "path": path.display().to_string(),
                    "glob": "["
                }),
                &test_ctx(tmp.path(), session_id),
            )
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("Invalid glob"));
    }

    #[tokio::test]
    async fn xgrep_reports_no_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "xgrep-no-match";
        clear_session_xfile_storage(session_id);
        let path = tmp.path().join("sample.txt");
        tokio::fs::write(&path, "alpha\nbeta\n").await.unwrap();

        let tool = XGrepTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "pattern": "todo",
                    "path": path.display().to_string()
                }),
                &test_ctx(tmp.path(), session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "No matches found.");
    }

    #[tokio::test]
    async fn xgrep_respects_limit_and_case_insensitive_search() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "xgrep-limit-case";
        clear_session_xfile_storage(session_id);
        let path = tmp.path().join("sample.txt");
        tokio::fs::write(&path, "TODO one\nTodo two\ntodo three\n")
            .await
            .unwrap();

        let tool = XGrepTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "pattern": "todo",
                    "path": path.display().to_string(),
                    "case_sensitive": false,
                    "limit": 2
                }),
                &test_ctx(tmp.path(), session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("TODO one"));
        assert!(result.content.contains("Todo two"));
        assert!(!result.content.contains("todo three"));
        assert!(result.content.contains("[more matches found, capped to 2]"));
    }

    #[tokio::test]
    async fn xgrep_includes_before_and_after_context() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "xgrep-context";
        clear_session_xfile_storage(session_id);
        let path = tmp.path().join("sample.txt");
        let head = store_written_text(session_id, &path, "zero\none\ntodo two\nthree\nfour\n");
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XGrepTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "pattern": "todo",
                    "path": path.display().to_string(),
                    "before": 1,
                    "after": 1
                }),
                &test_ctx(tmp.path(), session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains(":one"));
        assert!(result.content.contains(":todo two"));
        assert!(result.content.contains(":three"));
        assert!(!result.content.contains(":zero"));
        assert!(!result.content.contains(":four"));
    }

    #[tokio::test]
    async fn xgrep_merges_overlapping_context_windows() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "xgrep-overlap";
        clear_session_xfile_storage(session_id);
        let path = tmp.path().join("sample.txt");
        let head = store_written_text(
            session_id,
            &path,
            "zero\ntodo one\nbetween\ntodo two\nfour\n",
        );
        tokio::fs::write(&path, &head.rendered_content)
            .await
            .unwrap();

        let tool = XGrepTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "pattern": "todo",
                    "path": path.display().to_string(),
                    "before": 1,
                    "after": 1
                }),
                &test_ctx(tmp.path(), session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            result
                .content
                .lines()
                .filter(|line| line.ends_with(":between"))
                .count(),
            1
        );
        assert!(result.content.contains(":zero"));
        assert!(result.content.contains(":four"));
    }
}
