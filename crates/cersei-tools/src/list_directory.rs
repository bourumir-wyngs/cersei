//! ListDirectory tool: list folder contents with metadata, optionally recursive.

use super::*;
use serde::Deserialize;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

pub struct ListDirectoryTool;

#[async_trait]
impl Tool for ListDirectoryTool {
    fn name(&self) -> &str { "ListDirectory" }

    fn description(&self) -> &str {
        "List files and directories with metadata (size, permissions, owner). \
        Optionally recursive. Results are capped at a configurable limit (default 500). \
        Recursive mode is restricted to the workspace root."
    }

    fn permission_level(&self) -> PermissionLevel { PermissionLevel::ReadOnly }
    fn category(&self) -> ToolCategory { ToolCategory::FileSystem }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to list. Defaults to working directory."
                },
                "recursive": {
                    "type": "boolean",
                    "description": "Recurse into subdirectories. Restricted to workspace root. Default false."
                },
                "pattern": {
                    "type": "string",
                    "description": "Optional regex pattern to filter entries by filename/path. Applied to the relative path."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of entries to return. Default 500."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct Input {
            path: Option<String>,
            recursive: Option<bool>,
            pattern: Option<String>,
            limit: Option<usize>,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let filter = match input.pattern.as_deref() {
            Some(p) => match regex::Regex::new(p) {
                Ok(re) => Some(re),
                Err(e) => return ToolResult::error(format!("Invalid pattern: {}", e)),
            },
            None => None,
        };

        let target = input
            .path
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.working_dir.clone());

        let target = match target.canonicalize() {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Cannot access path: {}", e)),
        };

        let recursive = input.recursive.unwrap_or(false);
        let limit = input.limit.unwrap_or(500);

        // Security: recursive mode must stay within workspace root
        if recursive {
            let workspace = match ctx.working_dir.canonicalize() {
                Ok(p) => p,
                Err(_) => ctx.working_dir.clone(),
            };
            if !target.starts_with(&workspace) {
                return ToolResult::error(
                    "Recursive listing is only allowed within the workspace root.".to_string(),
                );
            }
        }

        if !target.is_dir() {
            return ToolResult::error(format!("'{}' is not a directory.", target.display()));
        }

        let mut entries: Vec<String> = Vec::new();
        let mut total = 0usize;
        let mut truncated = false;

        if recursive {
            collect_recursive(&target, &target, &filter, &mut entries, limit, &mut total, &mut truncated);
        } else {
            collect_flat(&target, &filter, &mut entries, limit, &mut total, &mut truncated);
        }

        let mut output = format!("{}:\n", target.display());
        output.push_str(&entries.join("\n"));
        if truncated {
            output.push_str(&format!("\n\n[truncated — showing first {} entries]", limit));
        }

        ToolResult::success(output)
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn format_entry(path: &std::path::Path, base: &std::path::Path) -> String {
    let rel = path.strip_prefix(base).unwrap_or(path);
    let display = rel.display().to_string();

    match std::fs::symlink_metadata(path) {
        Err(_) => format!("?? {}", display),
        Ok(meta) => {
            let is_dir = meta.is_dir();
            let is_link = meta.file_type().is_symlink();

            let size = if is_dir { "-".to_string() } else { format_size(meta.len()) };
            let perms = format_permissions(meta.permissions().mode());
            let owner = format_owner(meta.uid(), meta.gid());
            let type_char = if is_link { 'l' } else if is_dir { 'd' } else { 'f' };

            let name = if is_dir {
                format!("{}/", display)
            } else if is_link {
                // Show symlink target
                let target = std::fs::read_link(path)
                    .map(|t| t.display().to_string())
                    .unwrap_or_else(|_| "?".into());
                format!("{} -> {}", display, target)
            } else {
                display
            };

            format!("{} {:>8}  {}  {}  {}", type_char, size, perms, owner, name)
        }
    }
}

fn matches_filter(path: &std::path::Path, base: &std::path::Path, filter: &Option<regex::Regex>) -> bool {
    match filter {
        None => true,
        Some(re) => {
            let rel = path.strip_prefix(base).unwrap_or(path);
            re.is_match(&rel.display().to_string())
        }
    }
}

fn collect_flat(
    dir: &std::path::Path,
    filter: &Option<regex::Regex>,
    entries: &mut Vec<String>,
    limit: usize,
    total: &mut usize,
    truncated: &mut bool,
) {
    let mut raw: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.path()).collect(),
        Err(_) => return,
    };
    raw.sort();

    for path in raw {
        if !matches_filter(&path, dir, filter) {
            continue;
        }
        if *total >= limit {
            *truncated = true;
            return;
        }
        entries.push(format_entry(&path, dir));
        *total += 1;
    }
}

fn collect_recursive(
    dir: &std::path::Path,
    base: &std::path::Path,
    filter: &Option<regex::Regex>,
    entries: &mut Vec<String>,
    limit: usize,
    total: &mut usize,
    truncated: &mut bool,
) {
    if *truncated {
        return;
    }

    let mut raw: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.path()).collect(),
        Err(_) => return,
    };
    raw.sort();

    for path in raw {
        let is_dir = path.is_dir();

        if matches_filter(&path, base, filter) {
            if *total >= limit {
                *truncated = true;
                return;
            }
            entries.push(format_entry(&path, base));
            *total += 1;
        }

        if is_dir && !path.is_symlink() {
            collect_recursive(&path, base, filter, entries, limit, total, truncated);
        }
    }
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1}GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn format_permissions(mode: u32) -> String {
    let chars: Vec<char> = [
        (0o400, 'r'), (0o200, 'w'), (0o100, 'x'),
        (0o040, 'r'), (0o020, 'w'), (0o010, 'x'),
        (0o004, 'r'), (0o002, 'w'), (0o001, 'x'),
    ]
    .iter()
    .map(|(bit, ch)| if mode & bit != 0 { *ch } else { '-' })
    .collect();
    chars.into_iter().collect()
}

fn format_owner(uid: u32, gid: u32) -> String {
    use nix::unistd::{Gid, Group, Uid, User};

    let uname = User::from_uid(Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
        .unwrap_or_else(|| uid.to_string());

    let gname = Group::from_gid(Gid::from_raw(gid))
        .ok()
        .flatten()
        .map(|g| g.name)
        .unwrap_or_else(|| gid.to_string());

    format!("{}/{}", uname, gname)
}
