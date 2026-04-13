//! Exact-position and GNU sed-backed file editing with one-step session-local revert.

use super::*;
use crate::file_history::{unified_diff, FileHistory};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use similar::TextDiff;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tempfile::{NamedTempFile, TempDir};
use tokio::process::Command;

#[derive(Debug, Clone)]
struct LastEditSnapshot {
    file_path: PathBuf,
    content: Vec<u8>,
}

static LAST_EDIT_SNAPSHOT_REGISTRY: Lazy<dashmap::DashMap<String, LastEditSnapshot>> =
    Lazy::new(dashmap::DashMap::new);
static GNU_SED_BINARY: Lazy<std::result::Result<PathBuf, String>> = Lazy::new(probe_gnu_sed);
static PATCH_BINARY: Lazy<std::result::Result<PathBuf, String>> = Lazy::new(probe_patch);
static FIREJAIL_BINARY: Lazy<Option<PathBuf>> = Lazy::new(probe_firejail);
static FIREJAIL_FALLBACK_WARNED: AtomicBool = AtomicBool::new(false);

const INLINE_SED_SCRIPT_MAX_BYTES: usize = 1024;
const SED_STAGING_ROOT: &str = "/tmp";
const EXPECTED_TEXT_SEARCH_WINDOW_BYTES: usize = 256;
pub struct EditTool;
pub struct SedTool;
pub struct PatchTool;
pub struct RevertTool;

/// Public alias preserved for downstream imports.
pub type FileEditTool = EditTool;

#[derive(Debug, Clone, Deserialize)]
pub struct EditRequest {
    pub file_path: String,
    #[serde(default)]
    pub base_version: String,
    pub edits: Vec<TextEdit>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TextEdit {
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub expected_text: String,
    pub new_text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EditSuccess {
    pub ok: bool,
    pub file_path: String,
    pub old_version: String,
    pub new_version: String,
    pub applied_edits: usize,
    pub diff: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EditFailure {
    pub ok: bool,
    pub file_path: String,
    pub code: &'static str,
    pub message: String,
    pub edit_index: Option<usize>,
    pub current_version: Option<String>,
    pub actual_text: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct LineInfo {
    start: usize,
    end: usize,
    next_start: usize,
}

#[derive(Debug, Clone)]
struct DocumentIndex {
    lines: Vec<LineInfo>,
}

#[derive(Debug, Clone)]
struct ResolvedEdit {
    start: usize,
    end: usize,
    expected_text: String,
    new_text: String,
    source_index: usize,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Apply exact, byte-precise edits to an existing UTF-8 file using 1-based line/column \
         positions. Every request must provide a strict base_version hash plus the exact \
         expected_text for each edit. Read returns version metadata to bootstrap the first exact edit. A missing base_version is allowed only on the first Edit call for a file in a session; later calls must provide version metadata. Returns structured JSON."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file relative to the current workspace root. Absolute paths and `..` segments are not allowed."
                },
                "base_version": {
                    "type": "string",
                    "description": "Exact file version hash in the form `blake3:<hex>`. Read returns this in metadata for Edit. You may omit it only on the first Edit call for a file in a session; after that, provide version metadata. Requests fail if the file has changed."
                },
                "edits": {
                    "type": "array",
                    "description": "Edits to apply. Positions are 1-based. Columns count Unicode scalar values within a line.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "start_line": { "type": "integer", "minimum": 1 },
                            "start_column": { "type": "integer", "minimum": 1 },
                            "end_line": { "type": "integer", "minimum": 1 },
                            "end_column": { "type": "integer", "minimum": 1 },
                            "expected_text": { "type": "string" },
                            "new_text": { "type": "string" }
                        },
                        "required": [
                            "start_line",
                            "start_column",
                            "end_line",
                            "end_column",
                            "expected_text",
                            "new_text"
                        ]
                    }
                }
            },
            "required": ["file_path", "edits"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let raw_input = input.clone();
        let req: EditRequest = match serde_json::from_value(input) {
            Ok(req) => req,
            Err(err) => {
                let failure =
                    invalid_request_failure(&raw_input, format!("Invalid input: {}", err));
                return ToolResult::error(serialize_failure(&failure));
            }
        };

        let path = match resolve_edit_path(ctx, &req.file_path) {
            Ok(path) => path,
            Err(failure) => return ToolResult::error(serialize_failure(&failure)),
        };

        match execute_edit_inner(req, &path, Some(ctx)).await {
            Ok(payload) => ToolResult::success(payload),
            Err(failure) => ToolResult::error(serialize_failure(&failure)),
        }
    }
}

#[derive(Debug, Deserialize)]
struct PatchInput {
    patch: String,
}

#[async_trait]
impl Tool for PatchTool {
    fn name(&self) -> &str {
        "Patch"
    }

    fn description(&self) -> &str {
        "Apply a standard diff patch supplied directly in the `patch` input string. Use normal unified/context diff text with standard headers such as `diff --git`, `---`, and `+++`; do not create a temp file or invoke shell patch commands yourself. The tool validates syntax and workspace-relative target paths, dry-runs before writing, returns an explanatory error when malformed or not applicable, and prints the complete patch when it succeeds."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "Patch text to apply directly. Provide the complete unified/context diff in this field; do not create a temp file or wrap it in shell commands. Prefer standard headers like diff --git with a/... and b/... paths, or plain ---/+++ headers. The tool validates that target paths stay within the workspace, dry-runs the patch before writing, rejects malformed or non-applicable patches with an explanatory error, and echoes the complete applied patch on success."
                }
            },
            "required": ["patch"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let input: PatchInput = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        match apply_patch(ctx, &input.patch).await {
            Ok(message) => ToolResult::success(message),
            Err(message) => ToolResult::error(message),
        }
    }
}

#[async_trait]
impl Tool for SedTool {
    fn name(&self) -> &str {
        "Sed"
    }

    fn description(&self) -> &str {
        "Apply a real GNU sed script to one workspace file. The tool invokes the system `sed` \
         binary as `sed -E --sandbox`, so standard GNU sed commands, addresses, ranges, and \
         capture groups work, but file-access commands such as `e`, `r`, and `w` are disabled. \
         Provide only the sed args in `script`; do not include `sed`, `-i`, or filenames \
         because the tool supplies the target file and writes the resulting stdout back to it. \
         Use `quiet=true` for `-n` semantics when the script prints explicitly, and `null_data=true` \
         for `-z` / NUL-delimited processing. The tool returns a unified diff plus the reminder \"use 'revert' command if wrong\"."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file relative to the current workspace root. Absolute paths and `..` segments are not allowed."
                },
                "script": {
                    "type": "string",
                    "description": "Sed program only, executed as real GNU `sed -E --sandbox`. Use normal GNU sed syntax such as `s/foo/bar/g`, `1,20s/^/\\/\\/ /`, `/BEGIN/,/END/d`, or `s/(foo)/[\\\\1]/g`. Do not include the `sed` command itself, `-i`, extra filenames, or shell redirection. `e`, `r`, and `w` commands are rejected by `--sandbox`."
                },
                "quiet": {
                    "type": "boolean",
                    "description": "Equivalent to GNU sed `-n`. When true, only explicit print commands such as `p` contribute to the rewritten file contents.",
                    "default": false
                },
                "null_data": {
                    "type": "boolean",
                    "description": "Equivalent to GNU sed `-z`; process NUL-delimited records instead of newline-delimited lines.",
                    "default": false
                }
            },
            "required": ["file_path", "script"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct Input {
            file_path: String,
            script: String,
            #[serde(default)]
            quiet: bool,
            #[serde(default)]
            null_data: bool,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let path = match resolve_existing_workspace_path(ctx, &input.file_path) {
            Ok(path) => path,
            Err(err) => return err,
        };

        let original_bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) => return ToolResult::error(format!("Failed to read file: {}", e)),
        };
        let updated_bytes = match run_sed_script(
            ctx,
            &path,
            &input.script,
            input.quiet,
            input.null_data,
            &original_bytes,
        )
        .await
        {
            Ok(output) => output,
            Err(e) => return ToolResult::error(e),
        };
        let original_text = String::from_utf8_lossy(&original_bytes).into_owned();
        let updated_text = String::from_utf8_lossy(&updated_bytes).into_owned();

        if updated_bytes == original_bytes {
            return ToolResult::success(format!(
                "sed script produced no changes in {}.",
                path.display()
            ));
        };

        let snapshot = LastEditSnapshot {
            file_path: path.clone(),
            content: original_bytes.clone(),
        };
        let previous_snapshot =
            LAST_EDIT_SNAPSHOT_REGISTRY.insert(ctx.session_id.clone(), snapshot);

        if let Err(e) = write_bytes(&path, &updated_bytes).await {
            restore_previous_snapshot(&ctx.session_id, previous_snapshot);
            return ToolResult::error(format!("Failed to write file: {}", e));
        }

        if let Some(history) = ctx.extensions.get::<FileHistory>() {
            history.record_change(&path, Some(&original_text), &updated_text, "edit");
        }

        let diff = unified_diff(
            &original_text,
            &updated_text,
            &format!("{} (before)", path.display()),
            &format!("{} (after)", path.display()),
        );

        ToolResult::success(format!(
            "Applied sed script to {}.\n{}\n\nuse 'revert' command if wrong",
            path.display(),
            diff.trim_end()
        ))
    }
}

#[async_trait]
impl Tool for RevertTool {
    fn name(&self) -> &str {
        "Revert"
    }

    fn description(&self) -> &str {
        "Restore the previous checkpoint from the most recent successful `Edit` or `Sed` \
         edit in this session. Only one checkpoint is retained."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Optional safety check. If provided, it must be a workspace-relative path matching the file from the most recent edit."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize, Default)]
        struct Input {
            file_path: Option<String>,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let snapshot = match LAST_EDIT_SNAPSHOT_REGISTRY.get(&ctx.session_id) {
            Some(entry) => entry.clone(),
            None => {
                return ToolResult::error(
                    "No edit snapshot is available to revert. Run `Edit` or `Sed` first.",
                )
            }
        };

        if let Some(requested) = input.file_path.as_ref() {
            let requested_path = match resolve_workspace_path(ctx, requested, false) {
                Ok(path) => path,
                Err(err) => return err,
            };
            if requested_path != snapshot.file_path {
                return ToolResult::error(format!(
                    "The last edit snapshot is for {}, not {}.",
                    snapshot.file_path.display(),
                    requested
                ));
            }
        }

        let current_bytes = tokio::fs::read(&snapshot.file_path)
            .await
            .unwrap_or_default();
        let current_text = String::from_utf8_lossy(&current_bytes).into_owned();
        let restored_text = String::from_utf8_lossy(&snapshot.content).into_owned();

        if let Err(e) = write_bytes(&snapshot.file_path, &snapshot.content).await {
            return ToolResult::error(format!("Failed to write file: {}", e));
        }

        if let Some(history) = ctx.extensions.get::<FileHistory>() {
            history.record_change(
                &snapshot.file_path,
                Some(&current_text),
                &restored_text,
                "revert",
            );
        }

        LAST_EDIT_SNAPSHOT_REGISTRY.remove(&ctx.session_id);

        let diff = unified_diff(
            &current_text,
            &restored_text,
            &format!("{} (current)", snapshot.file_path.display()),
            &format!("{} (reverted)", snapshot.file_path.display()),
        );

        ToolResult::success(format!(
            "Reverted {} to the previous edit snapshot.\n{}",
            snapshot.file_path.display(),
            diff.trim_end()
        ))
    }
}

async fn run_sed_script(
    ctx: &ToolContext,
    path: &Path,
    script: &str,
    quiet: bool,
    null_data: bool,
    original_bytes: &[u8],
) -> std::result::Result<Vec<u8>, String> {
    let staged_dir = create_sed_staging_dir()?;
    let file_name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| std::ffi::OsStr::new("input"));
    let staged_path = staged_dir.path().join(file_name);

    tokio::fs::write(&staged_path, original_bytes)
        .await
        .map_err(|e| format!("Failed to stage file for sed: {}", e))?;

    let staged_script_path = if script.as_bytes().len() > INLINE_SED_SCRIPT_MAX_BYTES {
        let path = staged_dir.path().join("program.sed");
        tokio::fs::write(&path, script)
            .await
            .map_err(|e| format!("Failed to stage sed script: {}", e))?;
        Some(path)
    } else {
        None
    };

    let mut cmd = sed_command(
        ctx,
        &staged_path,
        script,
        staged_script_path.as_deref(),
        quiet,
        null_data,
    )?;
    let output = cmd
        .output()
        .await
        .map_err(|e| format!("Failed to launch sandboxed sed: {}", e))?;

    if output.status.success() {
        Ok(output.stdout)
    } else {
        let detail = process_failure_detail(&output.status, &output.stderr);
        Err(format!("Failed to run sed script: {}", detail))
    }
}

fn create_sed_staging_dir() -> std::result::Result<tempfile::TempDir, String> {
    tempfile::Builder::new()
        .prefix("cersei-sed-")
        .tempdir_in(SED_STAGING_ROOT)
        .or_else(|_| tempfile::Builder::new().prefix("cersei-sed-").tempdir())
        .map_err(|e| format!("Failed to create sed staging directory: {}", e))
}

async fn apply_patch(ctx: &ToolContext, patch_text: &str) -> std::result::Result<String, String> {
    if patch_text.trim().is_empty() {
        return Err("Patch input is empty.".to_string());
    }

    let parsed = parse_patch_manifest(patch_text)?;
    if parsed.targets.is_empty() {
        return Err(
            "Patch does not contain any file targets. Expected standard diff headers like ---/+++ or diff --git."
                .to_string(),
        );
    }

    let patch_bin = patch_binary()?;
    let staging_dir = create_patch_staging_dir()?;
    stage_patch_targets(ctx, &staging_dir, &parsed.targets).await?;

    let patch_file = staging_dir.path().join("input.patch");
    tokio::fs::write(&patch_file, patch_text)
        .await
        .map_err(|e| format!("Failed to write staged patch file: {}", e))?;

    let dry_run = run_patch_command(ctx, &patch_bin, staging_dir.path(), &patch_file, true).await?;
    if !dry_run.status.success() {
        return Err(explain_patch_failure(
            "Patch could not be applied",
            patch_text,
            &dry_run,
        ));
    }

    let apply = run_patch_command(ctx, &patch_bin, staging_dir.path(), &patch_file, false).await?;
    if !apply.status.success() {
        return Err(explain_patch_failure(
            "Patch failed while being applied after passing dry-run",
            patch_text,
            &apply,
        ));
    }

    let mut changed_files = Vec::new();
    let mut previous_snapshot: Option<LastEditSnapshot> = None;
    for target in &parsed.targets {
        let workspace_path = ctx.working_dir.join(&target.workspace_rel);
        let staged_path = staging_dir.path().join(&target.workspace_rel);
        let old_bytes = tokio::fs::read(&workspace_path)
            .await
            .map_err(|e| format!("Failed to read {}: {}", target.workspace_rel, e))?;
        let new_bytes = tokio::fs::read(&staged_path)
            .await
            .map_err(|e| format!("Failed to read staged {}: {}", target.workspace_rel, e))?;
        if old_bytes == new_bytes {
            continue;
        }

        let old_text = String::from_utf8_lossy(&old_bytes).into_owned();
        let new_text = String::from_utf8_lossy(&new_bytes).into_owned();
        let snapshot = LastEditSnapshot {
            file_path: workspace_path.clone(),
            content: old_bytes.clone(),
        };
        let prior = LAST_EDIT_SNAPSHOT_REGISTRY.insert(ctx.session_id.clone(), snapshot);
        if previous_snapshot.is_none() {
            previous_snapshot = prior;
        }

        if let Err(err) = write_bytes(&workspace_path, &new_bytes).await {
            restore_previous_snapshot(&ctx.session_id, previous_snapshot);
            return Err(format!("Failed to write {}: {}", target.workspace_rel, err));
        }

        if let Some(history) = ctx.extensions.get::<FileHistory>() {
            history.record_change(&workspace_path, Some(&old_text), &new_text, "patch");
        }
        changed_files.push(target.workspace_rel.clone());
    }

    let changed_summary = if changed_files.is_empty() {
        "Patch applied successfully but produced no file changes.".to_string()
    } else {
        format!("Applied patch to: {}", changed_files.join(", "))
    };

    Ok(format!(
        "{}\n\nComplete patch applied:\n{}",
        changed_summary, patch_text
    ))
}

fn parse_patch_manifest(patch_text: &str) -> std::result::Result<ParsedPatch, String> {
    let mut git_old: Option<String> = None;
    let mut git_new: Option<String> = None;
    let mut old_header: Option<String> = None;
    let mut saw_file_header = false;
    let mut saw_hunk = false;
    let mut targets = Vec::new();

    for line in patch_text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() < 2 {
                return Err(format!("Malformed diff --git header: {}", line));
            }
            git_old = Some(parts[0].to_string());
            git_new = Some(parts[1].to_string());
            saw_file_header = true;
            continue;
        }
        if let Some(path) = line.strip_prefix("--- ") {
            old_header = Some(extract_patch_path(path));
            saw_file_header = true;
            continue;
        }
        if let Some(path) = line.strip_prefix("+++ ") {
            let new_header = extract_patch_path(path);
            let target_path = choose_patch_target(
                git_old.as_deref(),
                git_new.as_deref(),
                old_header.as_deref(),
                Some(new_header.as_str()),
            )?;
            let workspace_rel = normalize_patch_workspace_path(&target_path)?;
            if !targets.iter().any(|t: &PatchTarget| t.workspace_rel == workspace_rel) {
                targets.push(PatchTarget { workspace_rel });
            }
            git_old = None;
            git_new = None;
            old_header = None;
            continue;
        }
        if line.starts_with("@@ ") || line.starts_with("***************") {
            saw_hunk = true;
        }
    }

    if !saw_file_header {
        return Err(
            "Patch is missing file headers. Expected standard diff headers like ---/+++ or diff --git."
                .to_string(),
        );
    }
    if !saw_hunk {
        return Err(
            "Patch is missing hunk content. Expected unified/context diff hunks such as @@ ... @@."
                .to_string(),
        );
    }

    Ok(ParsedPatch { targets })
}

fn extract_patch_path(header_value: &str) -> String {
    header_value
        .split(|c: char| c == '\t' || c == ' ')
        .next()
        .unwrap_or("")
        .to_string()
}

fn choose_patch_target(
    git_old: Option<&str>,
    git_new: Option<&str>,
    old_header: Option<&str>,
    new_header: Option<&str>,
) -> std::result::Result<String, String> {
    for candidate in [git_new, new_header, git_old, old_header]
        .into_iter()
        .flatten()
    {
        if candidate != "/dev/null" {
            return Ok(strip_patch_prefix(candidate).to_string());
        }
    }
    Err("Patch file header does not identify a workspace file target.".to_string())
}

fn strip_patch_prefix(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

fn normalize_patch_workspace_path(path: &str) -> std::result::Result<String, String> {
    if path.is_empty() {
        return Err("Patch contains an empty file path header.".to_string());
    }

    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Err(format!(
            "Patch path '{}' is absolute; only workspace-relative paths are allowed.",
            path
        ));
    }
    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "Patch path '{}' attempts path traversal with '..', which is not allowed.",
            path
        ));
    }

    let normalized = candidate
        .components()
        .filter_map(|component| match component {
            Component::Normal(segment) => Some(segment.to_os_string()),
            Component::CurDir => None,
            Component::ParentDir => None,
            Component::RootDir => None,
            _ => None,
        })
        .collect::<PathBuf>();
    if normalized.as_os_str().is_empty() {
        return Err("Patch path resolves to an empty workspace-relative path.".to_string());
    }

    Ok(normalized.to_string_lossy().into_owned())
}

async fn stage_patch_targets(
    ctx: &ToolContext,
    staging_dir: &TempDir,
    targets: &[PatchTarget],
) -> std::result::Result<(), String> {
    for target in targets {
        let workspace_path = resolve_existing_workspace_path(ctx, &target.workspace_rel)
            .map_err(|err| err.content)?;
        let staged_path = staging_dir.path().join(&target.workspace_rel);
        if let Some(parent) = staged_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to prepare patch staging directory: {}", e))?;
        }
        tokio::fs::copy(&workspace_path, &staged_path)
            .await
            .map_err(|e| format!("Failed to stage {}: {}", target.workspace_rel, e))?;
    }
    Ok(())
}

fn create_patch_staging_dir() -> std::result::Result<TempDir, String> {
    tempfile::Builder::new()
        .prefix("cersei-patch-")
        .tempdir_in(SED_STAGING_ROOT)
        .or_else(|_| tempfile::Builder::new().prefix("cersei-patch-").tempdir())
        .map_err(|e| format!("Failed to create patch staging directory: {}", e))
}

async fn run_patch_command(
    ctx: &ToolContext,
    patch_bin: &Path,
    staging_root: &Path,
    patch_file: &Path,
    dry_run: bool,
) -> std::result::Result<std::process::Output, String> {
    let output = if let Some(firejail) = firejail_binary() {
        let mut cmd = Command::new(firejail);
        append_firejail_prefix(&mut cmd, Some(&ctx.working_dir));
        cmd.arg("--");
        cmd.arg(patch_bin);
        append_patch_arguments(&mut cmd, staging_root, patch_file, dry_run);
        cmd.output()
            .await
            .map_err(|e| format!("Failed to run patch in firejail: {}", e))?
    } else {
        let mut cmd = Command::new(patch_bin);
        append_patch_arguments(&mut cmd, staging_root, patch_file, dry_run);
        cmd.output()
            .await
            .map_err(|e| format!("Failed to run patch: {}", e))?
    };

    Ok(output)
}

fn append_patch_arguments(
    cmd: &mut Command,
    staging_root: &Path,
    patch_file: &Path,
    dry_run: bool,
) {
    cmd.arg("--batch");
    cmd.arg("--forward");
    cmd.arg("--strip=1");
    cmd.arg("--directory");
    cmd.arg(staging_root);
    cmd.arg("--input");
    cmd.arg(patch_file);
    if dry_run {
        cmd.arg("--dry-run");
    }
}

fn patch_binary() -> std::result::Result<PathBuf, String> {
    match &*PATCH_BINARY {
        Ok(path) => Ok(path.clone()),
        Err(err) => Err(err.clone()),
    }
}

fn probe_patch() -> std::result::Result<PathBuf, String> {
    let patch = which::which("patch")
        .map_err(|_| "System `patch` binary not found in PATH; install patch on the host OS.".to_string())?;
    let output = std::process::Command::new(&patch)
        .arg("--version")
        .output()
        .map_err(|e| format!("Failed to probe patch at {}: {}", patch.display(), e))?;
    if !output.status.success() {
        return Err(format!(
            "Failed to invoke patch at {}: {}",
            patch.display(),
            process_failure_detail(&output.status, &output.stderr)
        ));
    }
    Ok(patch)
}

fn explain_patch_failure(
    prefix: &str,
    patch_text: &str,
    output: &std::process::Output,
) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        process_failure_detail(&output.status, &output.stderr)
    };
    format!("{}: {}\n\nPatch was:\n{}", prefix, detail, patch_text)
}

#[derive(Debug)]
struct ParsedPatch {
    targets: Vec<PatchTarget>,
}

#[derive(Debug)]
struct PatchTarget {
    workspace_rel: String,
}

fn sed_command(
    ctx: &ToolContext,
    staged_path: &Path,
    script: &str,
    staged_script_path: Option<&Path>,
    quiet: bool,
    null_data: bool,
) -> std::result::Result<Command, String> {
    if let Some(firejail) = firejail_binary() {
        return firejail_sed_command(
            ctx,
            &firejail,
            staged_path,
            script,
            staged_script_path,
            quiet,
            null_data,
        );
    }

    direct_sed_command(staged_path, script, staged_script_path, quiet, null_data)
}

fn append_firejail_prefix(cmd: &mut Command, blacklisted_dir: Option<&Path>) {
    cmd.arg("--quiet");
    cmd.arg("--noprofile");
    cmd.arg("--net=none");
    cmd.arg("--private");
    cmd.arg("--private-cache");
    cmd.arg("--private-dev");
    cmd.arg("--private-cwd=/");

    if let Some(working_dir) = blacklisted_dir.and_then(|path| path.canonicalize().ok()) {
        cmd.arg(format!("--blacklist={}", working_dir.display()));
    }

    cmd.arg("--caps.drop=all");
    cmd.arg("--nonewprivs");
    cmd.arg("--nodbus");
    cmd.arg("--x11=none");
    cmd.arg("--noinput");
    cmd.arg("--nosound");
    cmd.arg("--nou2f");
}

fn firejail_sed_command(
    ctx: &ToolContext,
    firejail: &Path,
    staged_path: &Path,
    script: &str,
    staged_script_path: Option<&Path>,
    quiet: bool,
    null_data: bool,
) -> std::result::Result<Command, String> {
    let sed = gnu_sed_binary()?;
    let mut cmd = Command::new(firejail);

    append_firejail_prefix(&mut cmd, Some(&ctx.working_dir));
    cmd.arg("--");
    cmd.arg(sed);
    append_sed_arguments(
        &mut cmd,
        staged_path,
        script,
        staged_script_path,
        quiet,
        null_data,
    );
    Ok(cmd)
}

fn direct_sed_command(
    staged_path: &Path,
    script: &str,
    staged_script_path: Option<&Path>,
    quiet: bool,
    null_data: bool,
) -> std::result::Result<Command, String> {
    let sed = gnu_sed_binary()?;
    let mut cmd = Command::new(sed);
    append_sed_arguments(
        &mut cmd,
        staged_path,
        script,
        staged_script_path,
        quiet,
        null_data,
    );
    Ok(cmd)
}

fn append_sed_arguments(
    cmd: &mut Command,
    staged_path: &Path,
    script: &str,
    staged_script_path: Option<&Path>,
    quiet: bool,
    null_data: bool,
) {
    cmd.arg("-E");
    cmd.arg("--sandbox");
    if quiet {
        cmd.arg("--quiet");
    }
    if null_data {
        cmd.arg("--null-data");
    }
    if let Some(staged_script_path) = staged_script_path {
        cmd.arg("-f");
        cmd.arg(staged_script_path);
    } else {
        cmd.arg("--expression");
        cmd.arg(script);
    }
    cmd.arg("--");
    cmd.arg(staged_path);
}

fn gnu_sed_binary() -> std::result::Result<PathBuf, String> {
    match &*GNU_SED_BINARY {
        Ok(path) => Ok(path.clone()),
        Err(err) => Err(err.clone()),
    }
}

fn firejail_binary() -> Option<PathBuf> {
    FIREJAIL_BINARY.clone()
}

fn probe_gnu_sed() -> std::result::Result<PathBuf, String> {
    let sed = which::which("sed")
        .map_err(|_| "GNU sed not found in PATH; install `sed` on the host OS.".to_string())?;
    let output = std::process::Command::new(&sed)
        .arg("--help")
        .output()
        .map_err(|e| format!("Failed to probe sed at {}: {}", sed.display(), e))?;

    let help = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || !help.contains("--sandbox") {
        return Err(format!(
            "System sed at {} does not support `--sandbox`; GNU sed with sandbox support is required.",
            sed.display()
        ));
    }

    Ok(sed)
}

fn probe_firejail() -> Option<PathBuf> {
    let firejail = match which::which("firejail") {
        Ok(path) => path,
        Err(_) => return None,
    };
    let sed = match gnu_sed_binary() {
        Ok(path) => path,
        Err(_) => return None,
    };
    let staged_file =
        match NamedTempFile::new_in(SED_STAGING_ROOT).or_else(|_| NamedTempFile::new()) {
            Ok(file) => file,
            Err(err) => {
                warn_firejail_fallback(format!("probe file setup failed: {}", err));
                return None;
            }
        };
    if let Err(err) = std::fs::write(staged_file.path(), b"probe\n") {
        warn_firejail_fallback(format!("probe file setup failed: {}", err));
        return None;
    }

    let output = match std::process::Command::new(&firejail)
        .args([
            "--quiet",
            "--noprofile",
            "--net=none",
            "--private",
            "--private-cache",
            "--private-dev",
            "--private-cwd=/",
            "--caps.drop=all",
            "--nonewprivs",
            "--nodbus",
            "--x11=none",
            "--noinput",
            "--nosound",
            "--nou2f",
            "--",
        ])
        .arg(&sed)
        .arg("-E")
        .arg("--sandbox")
        .arg("--expression")
        .arg("s/probe/probe/")
        .arg("--")
        .arg(staged_file.path())
        .output()
    {
        Ok(output) => output,
        Err(err) => {
            warn_firejail_fallback(format!(
                "failed to probe firejail at {}: {}",
                firejail.display(),
                err
            ));
            return None;
        }
    };

    if output.status.success() && output.stdout == b"probe\n" {
        Some(firejail)
    } else {
        warn_firejail_fallback(format!(
            "firejail at {} cannot launch sandboxed sed ({})",
            firejail.display(),
            process_failure_detail(&output.status, &output.stderr)
        ));
        None
    }
}

fn process_failure_detail(status: &std::process::ExitStatus, stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();
    if stderr.is_empty() {
        match status.code() {
            Some(code) => format!("exit status {}", code),
            None => "terminated by signal".to_string(),
        }
    } else {
        stderr
    }
}

fn warn_firejail_fallback(detail: String) {
    if FIREJAIL_FALLBACK_WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "warning: SedTool firejail sandbox unavailable ({}); falling back to direct `sed --sandbox` on staged temp files.",
        detail
    );
}

pub async fn execute_edit(req: EditRequest) -> std::result::Result<String, String> {
    let path = PathBuf::from(&req.file_path);
    execute_edit_inner(req, &path, None)
        .await
        .map_err(|failure| serialize_failure(&failure))
}

async fn execute_edit_inner(
    req: EditRequest,
    path: &Path,
    ctx: Option<&ToolContext>,
) -> std::result::Result<String, EditFailure> {
    let (bytes, text, version) = read_file(path, &req.file_path).await?;

    let missing_base_version = req.base_version.trim().is_empty();
    if missing_base_version && !is_first_edit_for_session_file(ctx, path) {
        return Err(failure(
            &req.file_path,
            "VERSION_REQUIRED",
            format!(
                "Provide version metadata from Read before editing {} again.",
                req.file_path
            ),
            None,
            Some(version.clone()),
            None,
        ));
    }

    if !missing_base_version && version != req.base_version {
        return Err(failure(
            &req.file_path,
            "VERSION_MISMATCH",
            format!(
                "Base version mismatch: expected {}, found {}.",
                req.base_version, version
            ),
            None,
            Some(version),
            None,
        ));
    }

    let index = build_index(&text);
    let resolved = resolve_edits(&text, &index, &req.edits, &req.file_path, &version)?;
    let new_text = apply_edits(&text, &resolved);

    if new_text != text {
        write_atomic(path, &new_text, &req.file_path).await?;

        if let Some(ctx) = ctx {
            if let Some(history) = ctx.extensions.get::<FileHistory>() {
                history.record_change(&path.to_path_buf(), Some(&text), &new_text, "edit");
            }
            LAST_EDIT_SNAPSHOT_REGISTRY.insert(
                ctx.session_id.clone(),
                LastEditSnapshot {
                    file_path: path.to_path_buf(),
                    content: bytes,
                },
            );
        }
    }

    let new_version = compute_version(new_text.as_bytes());
    let diff = make_diff(&text, &new_text);

    Ok(serialize_success(&EditSuccess {
        ok: true,
        file_path: req.file_path,
        old_version: version,
        new_version,
        applied_edits: resolved.len(),
        diff,
    }))
}

fn is_first_edit_for_session_file(ctx: Option<&ToolContext>, path: &Path) -> bool {
    let Some(ctx) = ctx else {
        return false;
    };

    match LAST_EDIT_SNAPSHOT_REGISTRY.get(&ctx.session_id) {
        Some(snapshot) => snapshot.file_path != path,
        None => true,
    }
}

/// Compute a version hash from the exact file bytes.
fn compute_version(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

async fn read_file(
    path: &Path,
    file_path: &str,
) -> std::result::Result<(Vec<u8>, String, String), EditFailure> {
    let bytes = tokio::fs::read(path).await.map_err(|_| {
        failure(
            file_path,
            "FILE_NOT_FOUND",
            "Cannot read file",
            None,
            None,
            None,
        )
    })?;

    let version = compute_version(&bytes);
    let text = String::from_utf8(bytes.clone()).map_err(|_| {
        failure(
            file_path,
            "FILE_NOT_UTF8",
            "Invalid UTF-8",
            None,
            Some(version.clone()),
            None,
        )
    })?;

    Ok((bytes, text, version))
}

fn build_index(text: &str) -> DocumentIndex {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return DocumentIndex {
            lines: vec![LineInfo {
                start: 0,
                end: 0,
                next_start: 0,
            }],
        };
    }

    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let end = if i > start && bytes[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            lines.push(LineInfo {
                start,
                end,
                next_start: i + 1,
            });
            start = i + 1;
        }
        i += 1;
    }

    if start < bytes.len() {
        lines.push(LineInfo {
            start,
            end: bytes.len(),
            next_start: bytes.len(),
        });
    }

    DocumentIndex { lines }
}

fn position_to_offset(
    text: &str,
    index: &DocumentIndex,
    line: usize,
    column: usize,
) -> std::result::Result<usize, String> {
    if line == 0 || column == 0 {
        return Err("String indices are 1 based.".into());
    }

    if line == index.lines.len() + 1 {
        return if column == 1 {
            Ok(text.len())
        } else {
            Err(format!(
                "Line {} refers to EOF; only column 1 is valid there.",
                line
            ))
        };
    }

    let info = index.lines.get(line - 1).ok_or_else(|| {
        format!(
            "Line {} is out of range for a document with {} line(s).",
            line,
            index.lines.len()
        )
    })?;

    debug_assert!(info.end <= info.next_start);
    let line_text = &text[info.start..info.end];
    let char_count = line_text.chars().count();

    if column > char_count + 1 {
        return Err(format!(
            "Column {} is out of range for line {}; max column is {}.",
            column,
            line,
            char_count + 1
        ));
    }

    if column == char_count + 1 {
        return Ok(info.end);
    }

    let char_offset = column - 1;
    let byte_offset = if char_offset == 0 {
        0
    } else {
        line_text
            .char_indices()
            .nth(char_offset)
            .map(|(offset, _)| offset)
            .unwrap_or(info.end - info.start)
    };

    Ok(info.start + byte_offset)
}

fn floor_char_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn ceil_char_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset < text.len() && !text.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

fn unique_nearby_expected_match(
    text: &str,
    start: usize,
    end: usize,
    expected_text: &str,
) -> Option<(usize, usize)> {
    if expected_text.is_empty() {
        return None;
    }

    let search_start = floor_char_boundary(
        text,
        start.saturating_sub(EXPECTED_TEXT_SEARCH_WINDOW_BYTES),
    );
    let search_end = ceil_char_boundary(
        text,
        end.saturating_add(expected_text.len())
            .saturating_add(EXPECTED_TEXT_SEARCH_WINDOW_BYTES),
    );
    let search = &text[search_start..search_end];

    let mut matches = search.match_indices(expected_text);
    let (first_match, _) = matches.next()?;
    if matches.next().is_some() {
        return None;
    }

    let matched_start = search_start + first_match;
    Some((matched_start, matched_start + expected_text.len()))
}

fn mismatch_context(text: &str, start: usize, end: usize) -> String {
    let context_start = floor_char_boundary(text, start.saturating_sub(40));
    let context_end = ceil_char_boundary(text, end.saturating_add(40));
    text[context_start..context_end].escape_debug().to_string()
}

fn resolve_edits(
    text: &str,
    index: &DocumentIndex,
    edits: &[TextEdit],
    file_path: &str,
    current_version: &str,
) -> std::result::Result<Vec<ResolvedEdit>, EditFailure> {
    let mut resolved = Vec::with_capacity(edits.len());

    for (edit_index, edit) in edits.iter().enumerate() {
        let start = position_to_offset(text, index, edit.start_line, edit.start_column).map_err(
            |message| {
                failure(
                    file_path,
                    "INVALID_POSITION",
                    format!(
                        "Invalid start position for edit {}: {}",
                        edit_index, message
                    ),
                    Some(edit_index),
                    Some(current_version.to_string()),
                    None,
                )
            },
        )?;

        let end =
            position_to_offset(text, index, edit.end_line, edit.end_column).map_err(|message| {
                failure(
                    file_path,
                    "INVALID_POSITION",
                    format!("Invalid end position for edit {}: {}", edit_index, message),
                    Some(edit_index),
                    Some(current_version.to_string()),
                    None,
                )
            })?;

        if start > end {
            return Err(failure(
                file_path,
                "INVALID_RANGE",
                format!(
                    "Edit {} start position is after its end position.",
                    edit_index
                ),
                Some(edit_index),
                Some(current_version.to_string()),
                None,
            ));
        }

        let actual = &text[start..end];
        let (start, end) = if actual != edit.expected_text {
            if let Some((matched_start, matched_end)) =
                unique_nearby_expected_match(text, start, end, &edit.expected_text)
            {
                (matched_start, matched_end)
            } else {
                return Err(failure(
                    file_path,
                    "EXPECTED_TEXT_MISMATCH",
                    format!(
                        "Edit {} expected text does not match the current file content at byte range {}..{}. Nearby context: \"{}\".",
                        edit_index,
                        start,
                        end,
                        mismatch_context(text, start, end)
                    ),
                    Some(edit_index),
                    Some(current_version.to_string()),
                    Some(actual.to_string()),
                ));
            }
        } else {
            (start, end)
        };

        if &text[start..end] != edit.expected_text {
            return Err(failure(
                file_path,
                "EXPECTED_TEXT_MISMATCH",
                format!(
                    "Edit {} expected text does not match the current file content at byte range {}..{}. Nearby context: \"{}\".",
                    edit_index,
                    start,
                    end,
                    mismatch_context(text, start, end)
                ),
                Some(edit_index),
                Some(current_version.to_string()),
                Some(text[start..end].to_string()),
            ));
        }

        resolved.push(ResolvedEdit {
            start,
            end,
            expected_text: edit.expected_text.clone(),
            new_text: edit.new_text.clone(),
            source_index: edit_index,
        });
    }

    resolved.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then(a.end.cmp(&b.end))
            .then(a.source_index.cmp(&b.source_index))
    });

    for pair in resolved.windows(2) {
        let previous = &pair[0];
        let current = &pair[1];
        if current.start < previous.end || current.start == previous.start {
            return Err(failure(
                file_path,
                "OVERLAPPING_EDITS",
                format!(
                    "Edit {} overlaps or shares a start position with edit {}.",
                    current.source_index, previous.source_index
                ),
                Some(current.source_index),
                Some(current_version.to_string()),
                None,
            ));
        }
    }

    Ok(resolved)
}

fn apply_edits(content: &str, edits: &[ResolvedEdit]) -> String {
    let mut result = content.to_string();

    for edit in edits.iter().rev() {
        debug_assert_eq!(&result[edit.start..edit.end], edit.expected_text.as_str());
        result.replace_range(edit.start..edit.end, &edit.new_text);
    }

    result
}

async fn write_atomic(
    path: &Path,
    content: &str,
    file_path: &str,
) -> std::result::Result<(), EditFailure> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let mut tmp = NamedTempFile::new_in(parent)
        .map_err(|err| write_failure(file_path, format!("Cannot create temp file: {}", err)))?;

    if let Ok(metadata) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(tmp.path(), metadata.permissions());
    }

    tmp.write_all(content.as_bytes())
        .map_err(|err| write_failure(file_path, format!("Cannot write temp file: {}", err)))?;
    tmp.flush()
        .map_err(|err| write_failure(file_path, format!("Cannot flush temp file: {}", err)))?;
    tmp.as_file_mut()
        .sync_all()
        .map_err(|err| write_failure(file_path, format!("Cannot sync temp file: {}", err)))?;
    tmp.persist(path).map_err(|err| {
        write_failure(
            file_path,
            format!("Cannot persist temp file: {}", err.error),
        )
    })?;

    Ok(())
}

fn make_diff(old: &str, new: &str) -> String {
    TextDiff::from_lines(old, new).unified_diff().to_string()
}

fn write_failure(file_path: &str, message: String) -> EditFailure {
    failure(file_path, "WRITE_FAILED", message, None, None, None)
}

fn resolve_edit_path(ctx: &ToolContext, input: &str) -> std::result::Result<PathBuf, EditFailure> {
    let candidate = Path::new(input);
    if candidate.is_absolute() {
        return Err(failure(
            input,
            "INVALID_PATH",
            "Absolute paths are not allowed; use a workspace-relative path.",
            None,
            None,
            None,
        ));
    }
    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(failure(
            input,
            "INVALID_PATH",
            "Path traversal with `..` is not allowed.",
            None,
            None,
            None,
        ));
    }

    let path = ctx.working_dir.join(candidate);
    if !path.exists() {
        return Err(failure(
            input,
            "FILE_NOT_FOUND",
            "Cannot read file",
            None,
            None,
            None,
        ));
    }

    Ok(path)
}

fn failure(
    file_path: &str,
    code: &'static str,
    message: impl Into<String>,
    edit_index: Option<usize>,
    current_version: Option<String>,
    actual_text: Option<String>,
) -> EditFailure {
    EditFailure {
        ok: false,
        file_path: file_path.to_string(),
        code,
        message: message.into(),
        edit_index,
        current_version,
        actual_text,
    }
}

fn invalid_request_failure(input: &Value, message: String) -> EditFailure {
    let file_path = input
        .get("file_path")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    failure(&file_path, "INVALID_REQUEST", message, None, None, None)
}

fn serialize_success(success: &EditSuccess) -> String {
    serde_json::to_string(success).unwrap_or_else(|_| {
        "{\"ok\":false,\"file_path\":\"\",\"code\":\"SERIALIZATION_FAILED\",\"message\":\"Failed to serialize edit success payload\",\"edit_index\":null,\"current_version\":null,\"actual_text\":null}".into()
    })
}

fn serialize_failure(failure: &EditFailure) -> String {
    serde_json::to_string(failure).unwrap_or_else(|_| {
        "{\"ok\":false,\"file_path\":\"\",\"code\":\"SERIALIZATION_FAILED\",\"message\":\"Failed to serialize edit failure payload\",\"edit_index\":null,\"current_version\":null,\"actual_text\":null}".into()
    })
}

async fn write_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, bytes).await
}

fn resolve_existing_workspace_path(
    ctx: &ToolContext,
    input: &str,
) -> std::result::Result<PathBuf, ToolResult> {
    resolve_workspace_path(ctx, input, true)
}

fn resolve_workspace_path(
    ctx: &ToolContext,
    input: &str,
    require_exists: bool,
) -> std::result::Result<PathBuf, ToolResult> {
    let candidate = Path::new(input);
    if candidate.is_absolute() {
        return Err(ToolResult::error(
            "Absolute paths are not allowed; use a workspace-relative path.",
        ));
    }
    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ToolResult::error(
            "Path traversal with `..` is not allowed.",
        ));
    }

    let path = ctx.working_dir.join(candidate);
    if require_exists && !path.exists() {
        return Err(ToolResult::error(format!(
            "File not found: {}",
            path.display()
        )));
    }

    Ok(path)
}

fn restore_previous_snapshot(
    session_id: &str,
    previous: Option<LastEditSnapshot>,
) -> Option<LastEditSnapshot> {
    match previous {
        Some(snapshot) => LAST_EDIT_SNAPSHOT_REGISTRY.insert(session_id.to_string(), snapshot),
        None => LAST_EDIT_SNAPSHOT_REGISTRY
            .remove(session_id)
            .map(|(_, snapshot)| snapshot),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::AllowAll;
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn test_ctx(working_dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: working_dir.to_path_buf(),
            session_id: format!("edit-test-{}", Uuid::new_v4()),
            permissions: Arc::new(AllowAll),
            cost_tracker: Arc::new(CostTracker::new()),
            mcp_manager: None,
            extensions: Extensions::default(),
            network_policy: None,
        }
    }

    fn write_text(tmp: &TempDir, rel_path: &str, content: &str) {
        std::fs::write(tmp.path().join(rel_path), content).unwrap();
    }

    fn write_bytes_fixture(tmp: &TempDir, rel_path: &str, content: &[u8]) {
        std::fs::write(tmp.path().join(rel_path), content).unwrap();
    }

    fn read_text(tmp: &TempDir, rel_path: &str) -> String {
        std::fs::read_to_string(tmp.path().join(rel_path)).unwrap()
    }

    fn read_bytes_fixture(tmp: &TempDir, rel_path: &str) -> Vec<u8> {
        std::fs::read(tmp.path().join(rel_path)).unwrap()
    }

    async fn run_edit_request(
        ctx: &ToolContext,
        file_path: &str,
        base_version: String,
        edits: Vec<Value>,
    ) -> (ToolResult, Value) {
        let tool = EditTool;
        let result = tool
            .execute(
                json!({
                    "file_path": file_path,
                    "base_version": base_version,
                    "edits": edits,
                }),
                ctx,
            )
            .await;

        let payload: Value = serde_json::from_str(&result.content).unwrap();
        (result, payload)
    }

    async fn run_edit_request_without_version(
        ctx: &ToolContext,
        file_path: &str,
        edits: Vec<Value>,
    ) -> (ToolResult, Value) {
        let tool = EditTool;
        let result = tool
            .execute(
                json!({
                    "file_path": file_path,
                    "edits": edits,
                }),
                ctx,
            )
            .await;

        let payload: Value = serde_json::from_str(&result.content).unwrap();
        (result, payload)
    }

    async fn run_sed_request(
        ctx: &ToolContext,
        file_path: &str,
        script: &str,
        quiet: bool,
        null_data: bool,
    ) -> ToolResult {
        let tool = SedTool;
        tool.execute(
            json!({
                "file_path": file_path,
                "script": script,
                "quiet": quiet,
                "null_data": null_data,
            }),
            ctx,
        )
        .await
    }

    async fn run_patch_request(ctx: &ToolContext, patch: &str) -> ToolResult {
        let tool = PatchTool;
        tool.execute(json!({ "patch": patch }), ctx).await
    }

    fn ensure_sed_runtime() -> bool {
        match gnu_sed_binary() {
            Ok(_) => true,
            Err(err) => {
                eprintln!("skipping sed test: {err}");
                false
            }
        }
    }

    fn ensure_patch_runtime() -> bool {
        match patch_binary() {
            Ok(_) => true,
            Err(err) => {
                eprintln!("skipping patch test: {err}");
                false
            }
        }
    }

    #[tokio::test]
    async fn exact_replace() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version("hello world\n".as_bytes()),
            vec![json!({
                "start_line": 1,
                "start_column": 7,
                "end_line": 1,
                "end_column": 12,
                "expected_text": "world",
                "new_text": "there"
            })],
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(payload["ok"], json!(true));
        assert_eq!(payload["applied_edits"], json!(1));
        assert_eq!(read_text(&tmp, "sample.txt"), "hello there\n");
        assert_eq!(
            payload["new_version"],
            json!(compute_version("hello there\n".as_bytes()))
        );
    }

    #[tokio::test]
    async fn insertion() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());

        let (result, _) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version("hello world\n".as_bytes()),
            vec![json!({
                "start_line": 1,
                "start_column": 7,
                "end_line": 1,
                "end_column": 7,
                "expected_text": "",
                "new_text": "big "
            })],
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(read_text(&tmp, "sample.txt"), "hello big world\n");
    }

    #[tokio::test]
    async fn deletion() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello big world\n");
        let ctx = test_ctx(tmp.path());

        let (result, _) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version("hello big world\n".as_bytes()),
            vec![json!({
                "start_line": 1,
                "start_column": 7,
                "end_line": 1,
                "end_column": 11,
                "expected_text": "big ",
                "new_text": ""
            })],
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(read_text(&tmp, "sample.txt"), "hello world\n");
    }

    #[tokio::test]
    async fn multi_edit_atomic_success() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "alpha\nbeta\ngamma\n");
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version("alpha\nbeta\ngamma\n".as_bytes()),
            vec![
                json!({
                    "start_line": 2,
                    "start_column": 1,
                    "end_line": 2,
                    "end_column": 5,
                    "expected_text": "beta",
                    "new_text": "BETA"
                }),
                json!({
                    "start_line": 1,
                    "start_column": 1,
                    "end_line": 1,
                    "end_column": 6,
                    "expected_text": "alpha",
                    "new_text": "ALPHA"
                }),
                json!({
                    "start_line": 3,
                    "start_column": 1,
                    "end_line": 3,
                    "end_column": 1,
                    "expected_text": "",
                    "new_text": ">> "
                }),
            ],
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(payload["applied_edits"], json!(3));
        assert_eq!(read_text(&tmp, "sample.txt"), "ALPHA\nBETA\n>> gamma\n");
    }

    #[tokio::test]
    async fn overlap_rejection() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "abcdef\n");
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version("abcdef\n".as_bytes()),
            vec![
                json!({
                    "start_line": 1,
                    "start_column": 1,
                    "end_line": 1,
                    "end_column": 4,
                    "expected_text": "abc",
                    "new_text": "X"
                }),
                json!({
                    "start_line": 1,
                    "start_column": 2,
                    "end_line": 1,
                    "end_column": 5,
                    "expected_text": "bcd",
                    "new_text": "Y"
                }),
            ],
        )
        .await;

        assert!(result.is_error);
        assert_eq!(payload["code"], json!("OVERLAPPING_EDITS"));
        assert_eq!(read_text(&tmp, "sample.txt"), "abcdef\n");
    }

    #[tokio::test]
    async fn zero_indices_return_one_based_error() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version("hello world\n".as_bytes()),
            vec![json!({
                "start_line": 0,
                "start_column": 1,
                "end_line": 1,
                "end_column": 1,
                "expected_text": "",
                "new_text": "X"
            })],
        )
        .await;

        assert!(result.is_error);
        assert_eq!(payload["code"], json!("INVALID_POSITION"));
        assert_eq!(
            payload["message"],
            json!("Invalid start position for edit 0: String indices are 1 based.")
        );
        assert_eq!(read_text(&tmp, "sample.txt"), "hello world\n");
    }

    #[tokio::test]
    async fn version_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            "blake3:deadbeef".into(),
            vec![json!({
                "start_line": 1,
                "start_column": 7,
                "end_line": 1,
                "end_column": 12,
                "expected_text": "world",
                "new_text": "there"
            })],
        )
        .await;

        assert!(result.is_error);
        assert_eq!(payload["code"], json!("VERSION_MISMATCH"));
        assert_eq!(
            payload["current_version"],
            json!(compute_version("hello world\n".as_bytes()))
        );
        assert_eq!(read_text(&tmp, "sample.txt"), "hello world\n");
    }

    #[tokio::test]
    async fn expected_text_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version("hello world\n".as_bytes()),
            vec![json!({
                "start_line": 1,
                "start_column": 7,
                "end_line": 1,
                "end_column": 12,
                "expected_text": "earth",
                "new_text": "there"
            })],
        )
        .await;

        assert!(result.is_error);
        assert_eq!(payload["code"], json!("EXPECTED_TEXT_MISMATCH"));
        assert_eq!(payload["actual_text"], json!("world"));
        assert_eq!(read_text(&tmp, "sample.txt"), "hello world\n");
    }

    #[tokio::test]
    async fn crlf_preservation() {
        let tmp = tempfile::tempdir().unwrap();
        write_bytes_fixture(&tmp, "sample.txt", b"one\r\ntwo\r\n");
        let ctx = test_ctx(tmp.path());

        let (result, _) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version(b"one\r\ntwo\r\n"),
            vec![json!({
                "start_line": 2,
                "start_column": 1,
                "end_line": 2,
                "end_column": 4,
                "expected_text": "two",
                "new_text": "dos"
            })],
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(read_bytes_fixture(&tmp, "sample.txt"), b"one\r\ndos\r\n");
    }

    #[tokio::test]
    async fn unicode_correctness() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "a🙂z\n");
        let ctx = test_ctx(tmp.path());

        let (result, _) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version("a🙂z\n".as_bytes()),
            vec![json!({
                "start_line": 1,
                "start_column": 2,
                "end_line": 1,
                "end_column": 3,
                "expected_text": "🙂",
                "new_text": "ß"
            })],
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(read_text(&tmp, "sample.txt"), "aßz\n");
    }

    #[tokio::test]
    async fn eof_insertion() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "abc");
        let ctx = test_ctx(tmp.path());

        let (result, _) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version("abc".as_bytes()),
            vec![json!({
                "start_line": 2,
                "start_column": 1,
                "end_line": 2,
                "end_column": 1,
                "expected_text": "",
                "new_text": "\n"
            })],
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(read_text(&tmp, "sample.txt"), "abc\n");
    }

    #[tokio::test]
    async fn empty_file_insert() {
        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "");
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version(b""),
            vec![json!({
                "start_line": 1,
                "start_column": 1,
                "end_line": 1,
                "end_column": 1,
                "expected_text": "",
                "new_text": "hello\n"
            })],
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(payload["applied_edits"], json!(1));
        assert_eq!(read_text(&tmp, "sample.txt"), "hello\n");
    }

    #[tokio::test]
    async fn nearby_attribute_boundary_match_recovers_after_prior_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let initial = "\
pub struct EditTool;\n\
pub struct SedTool;\n\
\n\
#[async_trait]\n\
impl Tool for SedTool {\n\
    fn name(&self) -> &str {\n\
        \"Sed\"\n\
    }\n\
}\n";
        write_text(&tmp, "sample.txt", initial);
        let ctx = test_ctx(tmp.path());

        let (first_result, first_payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version(initial.as_bytes()),
            vec![json!({
                "start_line": 3,
                "start_column": 1,
                "end_line": 3,
                "end_column": 1,
                "expected_text": "",
                "new_text": "pub struct PatchTool;\n"
            })],
        )
        .await;

        assert!(!first_result.is_error, "{}", first_result.content);

        let after_first = read_text(&tmp, "sample.txt");
        let (second_result, second_payload) = run_edit_request(
            &ctx,
            "sample.txt",
            first_payload["new_version"].as_str().unwrap().to_string(),
            vec![json!({
                "start_line": 4,
                "start_column": 1,
                "end_line": 4,
                "end_column": 1,
                "expected_text": "#[async_trait]\nimpl Tool for SedTool {",
                "new_text": "#[derive(Deserialize)]\nstruct PatchInput {\n    patch: String,\n}\n\n#[async_trait]\nimpl Tool for SedTool {"
            })],
        )
        .await;

        assert!(!second_result.is_error, "{}", second_result.content);
        let updated = read_text(&tmp, "sample.txt");
        assert!(after_first.contains("#[async_trait]\nimpl Tool for SedTool {"));
        assert!(updated.contains("pub struct PatchTool;"));
        assert!(updated.contains("#[derive(Deserialize)]\nstruct PatchInput {\n    patch: String,\n}\n\n#[async_trait]\nimpl Tool for SedTool {"));
        assert_eq!(second_payload["applied_edits"], json!(1));
    }

    #[tokio::test]
    async fn nearby_function_boundary_match_recovers_from_newline_only_slice() {
        let tmp = tempfile::tempdir().unwrap();
        let initial = "\
fn create_sed_staging_dir() -> std::result::Result<(), String> {\n\
    Ok(())\n\
}\n\
\n\
fn sed_command() {}\n";
        write_text(&tmp, "sample.txt", initial);
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version(initial.as_bytes()),
            vec![json!({
                "start_line": 4,
                "start_column": 1,
                "end_line": 5,
                "end_column": 1,
                "expected_text": "fn sed_command(",
                "new_text": "fn patch_helper() {}\n\nfn sed_command("
            })],
        )
        .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(payload["applied_edits"], json!(1));
        assert_eq!(
            read_text(&tmp, "sample.txt"),
            "\
fn create_sed_staging_dir() -> std::result::Result<(), String> {\n\
    Ok(())\n\
}\n\
\n\
fn patch_helper() {}\n\
\n\
fn sed_command() {}\n"
        );
    }

    #[tokio::test]
    async fn nearby_match_remains_rejected_when_ambiguous() {
        let tmp = tempfile::tempdir().unwrap();
        let initial = "\
#[async_trait]\n\
impl Tool for SedTool {\n\
}\n\
\n\
#[async_trait]\n\
impl Tool for SedTool {\n\
}\n";
        write_text(&tmp, "sample.txt", initial);
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version(initial.as_bytes()),
            vec![json!({
                "start_line": 4,
                "start_column": 1,
                "end_line": 4,
                "end_column": 1,
                "expected_text": "#[async_trait]\nimpl Tool for SedTool {",
                "new_text": "ignored"
            })],
        )
        .await;

        assert!(result.is_error);
        assert_eq!(payload["code"], json!("EXPECTED_TEXT_MISMATCH"));
        assert_eq!(payload["actual_text"], json!(""));
        assert_eq!(read_text(&tmp, "sample.txt"), initial);
    }

    #[tokio::test]
    async fn sequential_boundary_edits_keep_versions_and_apply_nearby_match() {
        let tmp = tempfile::tempdir().unwrap();
        let initial = "\
alpha\n\
\n\
#[async_trait]\n\
impl Tool for SedTool {\n\
    fn name(&self) -> &str {\n\
        \"Sed\"\n\
    }\n\
}\n";
        write_text(&tmp, "sample.txt", initial);
        let ctx = test_ctx(tmp.path());

        let (first_result, first_payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version(initial.as_bytes()),
            vec![json!({
                "start_line": 2,
                "start_column": 1,
                "end_line": 2,
                "end_column": 1,
                "expected_text": "",
                "new_text": "pub struct PatchTool;\n"
            })],
        )
        .await;

        assert!(!first_result.is_error, "{}", first_result.content);
        let after_first = read_text(&tmp, "sample.txt");
        let first_version = first_payload["new_version"].as_str().unwrap().to_string();
        assert_eq!(first_version, compute_version(after_first.as_bytes()));

        let (second_result, second_payload) = run_edit_request(
            &ctx,
            "sample.txt",
            first_version.clone(),
            vec![json!({
                "start_line": 4,
                "start_column": 1,
                "end_line": 4,
                "end_column": 1,
                "expected_text": "#[async_trait]\nimpl Tool for SedTool {",
                "new_text": "#[derive(Deserialize)]\nstruct PatchInput {\n    patch: String,\n}\n\n#[async_trait]\nimpl Tool for SedTool {"
            })],
        )
        .await;

        assert!(!second_result.is_error, "{}", second_result.content);
        assert_eq!(second_payload["applied_edits"], json!(1));
        let final_text = read_text(&tmp, "sample.txt");
        assert!(final_text.contains("pub struct PatchTool;"));
        assert!(final_text.contains("#[derive(Deserialize)]\nstruct PatchInput {\n    patch: String,\n}\n\n#[async_trait]\nimpl Tool for SedTool {"));
        assert_eq!(
            second_payload["new_version"],
            json!(compute_version(final_text.as_bytes()))
        );
    }

    #[tokio::test]
    async fn mismatch_reports_context_for_newline_boundary_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let initial = "\
fn create_sed_staging_dir() -> std::result::Result<(), String> {\n\
    Ok(())\n\
}\n\
\n\
fn sed_command() {}\n";
        write_text(&tmp, "sample.txt", initial);
        let ctx = test_ctx(tmp.path());

        let (result, payload) = run_edit_request(
            &ctx,
            "sample.txt",
            compute_version(initial.as_bytes()),
            vec![json!({
                "start_line": 4,
                "start_column": 1,
                "end_line": 5,
                "end_column": 1,
                "expected_text": "fn missing_command(",
                "new_text": "ignored"
            })],
        )
        .await;

        assert!(result.is_error);
        assert_eq!(payload["code"], json!("EXPECTED_TEXT_MISMATCH"));
        assert_eq!(payload["actual_text"], json!("\n"));
        let message = payload["message"].as_str().unwrap();
        assert!(message.contains("Nearby context:"));
        assert!(message.contains("fn sed_command() {}"));
        assert_eq!(read_text(&tmp, "sample.txt"), initial);
    }

    #[tokio::test]
    async fn sed_successful_edit() {
        if !ensure_sed_runtime() {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());

        let result = run_sed_request(&ctx, "sample.txt", "s/world/there/\n", false, false).await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(read_text(&tmp, "sample.txt"), "hello there\n");
        assert!(result.content.contains("Applied sed script"));
    }

    #[tokio::test]
    async fn sed_blocks_file_access_commands() {
        if !ensure_sed_runtime() {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());

        let result = run_sed_request(&ctx, "sample.txt", "1r /etc/passwd\n", false, false).await;

        assert!(result.is_error);
        assert!(result
            .content
            .contains("e/r/w commands disabled in sandbox mode"));
        assert_eq!(read_text(&tmp, "sample.txt"), "hello world\n");
    }

    #[tokio::test]
    async fn sed_revert_compatibility() {
        if !ensure_sed_runtime() {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());

        let result = run_sed_request(&ctx, "sample.txt", "s/world/there/\n", false, false).await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(read_text(&tmp, "sample.txt"), "hello there\n");

        let revert_tool = RevertTool;
        let revert = revert_tool
            .execute(json!({ "file_path": "sample.txt" }), &ctx)
            .await;

        assert!(!revert.is_error, "{}", revert.content);
        assert_eq!(read_text(&tmp, "sample.txt"), "hello world\n");
    }

    #[tokio::test]
    async fn sed_large_script_transparently_uses_staged_file() {
        if !ensure_sed_runtime() {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());
        let filler = "#".repeat(1100);
        let script = format!("s/world/there/\n#{}\n", filler);

        let result = run_sed_request(&ctx, "sample.txt", &script, false, false).await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(read_text(&tmp, "sample.txt"), "hello there\n");
    }

    #[tokio::test]
    async fn patch_applies_standard_git_diff_and_prints_complete_patch() {
        if !ensure_patch_runtime() {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());
        let patch = "diff --git a/sample.txt b/sample.txt\n--- a/sample.txt\n+++ b/sample.txt\n@@ -1 +1 @@\n-hello world\n+hello there\n";

        let result = run_patch_request(&ctx, patch).await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(read_text(&tmp, "sample.txt"), "hello there\n");
        assert!(result.content.contains("Applied patch to: sample.txt"));
        assert!(result.content.contains("Complete patch applied:"));
        assert!(result.content.contains(patch));
    }

    #[tokio::test]
    async fn patch_rejects_invalid_syntax_with_explanatory_error() {
        if !ensure_patch_runtime() {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());
        let result = run_patch_request(&ctx, "diff --git a/sample.txt\n@@ -1 +1 @@\n").await;

        assert!(result.is_error);
        assert!(result.content.contains("Malformed diff --git header"));
        assert_eq!(read_text(&tmp, "sample.txt"), "hello world\n");
    }

    #[tokio::test]
    async fn patch_rejects_paths_outside_workspace() {
        if !ensure_patch_runtime() {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());
        let patch = "diff --git a/../../etc/passwd b/../../etc/passwd\n--- a/../../etc/passwd\n+++ b/../../etc/passwd\n@@ -1 +1 @@\n-root\n+user\n";

        let result = run_patch_request(&ctx, patch).await;

        assert!(result.is_error);
        assert!(result.content.contains("attempts path traversal"));
        assert_eq!(read_text(&tmp, "sample.txt"), "hello world\n");
    }

    #[tokio::test]
    async fn patch_reports_when_patch_cannot_apply() {
        if !ensure_patch_runtime() {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "hello world\n");
        let ctx = test_ctx(tmp.path());
        let patch = "diff --git a/sample.txt b/sample.txt\n--- a/sample.txt\n+++ b/sample.txt\n@@ -1 +1 @@\n-goodbye world\n+hello there\n";

        let result = run_patch_request(&ctx, patch).await;

        assert!(result.is_error);
        assert!(result.content.contains("Patch could not be applied"));
        assert!(result.content.contains("Patch was:"));
        assert_eq!(read_text(&tmp, "sample.txt"), "hello world\n");
    }

    #[tokio::test]
    async fn patch_edits_created_test_file_in_multiple_hunks() {
        if !ensure_patch_runtime() {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        write_text(&tmp, "sample.txt", "alpha\nbeta\ngamma\n");
        let ctx = test_ctx(tmp.path());
        let patch = "diff --git a/sample.txt b/sample.txt\n--- a/sample.txt\n+++ b/sample.txt\n@@ -1,3 +1,4 @@\n alpha\n-beta\n+beta patched\n gamma\n+delta\n";

        let result = run_patch_request(&ctx, patch).await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            read_text(&tmp, "sample.txt"),
            "alpha\nbeta patched\ngamma\ndelta\n"
        );
        assert!(result.content.contains("Applied patch to: sample.txt"));
        assert!(result.content.contains(patch));
    }

    #[tokio::test]
    async fn sed_supports_null_data() {
        if !ensure_sed_runtime() {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        write_bytes_fixture(&tmp, "sample.txt", b"alpha\0beta\0");
        let ctx = test_ctx(tmp.path());

        let result = run_sed_request(&ctx, "sample.txt", "s/beta/gamma/\n", false, true).await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(read_bytes_fixture(&tmp, "sample.txt"), b"alpha\0gamma\0");
    }
}
