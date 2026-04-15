//! Edit tool: session-scoped tagged line edits backed by XFileStorage.

use super::*;
use crate::xfile_storage::{
    apply_mutations, ensure_loaded, resolve_xfile_path, XFile, XLineMutation,
};
use serde::{Deserialize, Serialize};

pub struct XEditTool;

/// Public alias preserved for downstream imports.
pub type FileXEditTool = XEditTool;

#[derive(Debug, Clone, Deserialize)]
pub struct XEditRequest {
    pub file_path: String,
    #[serde(default)]
    pub base_version: Option<String>,
    #[serde(default)]
    pub operations: Vec<XEditOperation>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct XEditOperation {
    pub op: String,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub from_tag: Option<String>,
    #[serde(default)]
    pub front_tag: Option<String>,
    #[serde(default, alias = "to_tage")]
    pub to_tag: Option<String>,
    #[serde(default)]
    pub move_after_tag: Option<String>,
    #[serde(default)]
    pub new_text: Option<String>,
    #[serde(default)]
    pub new_lines: Option<Vec<String>>,
    #[serde(default)]
    pub new_content: Option<String>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub replacement: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct XEditSuccess {
    pub ok: bool,
    pub file_path: String,
    pub current_version: String,
    pub revision_count: usize,
    pub applied_operations: usize,
    pub diff: String,
}

#[async_trait]
impl Tool for XEditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Edit the latest session-scoped XFileStorage revision of a file using unique line tags instead of line numbers. Use tags returned by Read or Grep. Supported operations are `replace_line`, `insert_before`, `insert_after`, `delete_line`, `delete_range`, `move_range`, `overwrite_range`, and `regex_replace`. `replace_line` keeps the same tag. `move_range` preserves tags on moved lines. `overwrite_range` keeps tags for the overlapping leading lines in the replaced range, deletes tags for removed lines, and gives fresh tags to extra new lines. `regex_replace` applies a Rust `regex` pattern to each selected line individually, preserves every selected line tag, and must not create extra lines or delete lines. Pass `base_version` from `current_version` metadata returned by Read, Write, or Edit to avoid editing a stale revision. After success, Edit flushes the updated file to disk and returns `current_version`, `revision_count`, `applied_operations`, and a tag-based `diff`."
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
                    "description": "Path to the target file. Absolute paths and workspace-relative paths are accepted."
                },
                "base_version": {
                    "type": "string",
                    "description": "Optional optimistic-concurrency version marker. Use `current_version` metadata from Read, Write, or a previous Edit call. If provided and stale, Edit fails."
                },
                "operations": {
                    "type": "array",
                    "description": "Line-oriented edit operations addressed by unique tags. Operations are applied in order to the current XFileStorage head revision.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": {
                                "type": "string",
                                "enum": ["replace_line", "insert_before", "insert_after", "delete_line", "delete_range", "move_range", "overwrite_range", "regex_replace"],
                                "description": "Operation kind. `replace_line` needs `tag` and `new_text`. `insert_before` and `insert_after` need `tag` and `new_lines`. `delete_line` needs `tag`. `delete_range` needs `from_tag` and `to_tag`. `move_range` needs `front_tag` (or `from_tag`), `to_tag`, and `move_after_tag`. `overwrite_range` needs `from_tag`, `to_tag`, and `new_content`. `regex_replace` needs `from_tag`, non-empty `pattern`, and `replacement`; `to_tag` is optional and defaults to the same line as `from_tag`."
                            },
                            "tag": {
                                "type": "string",
                                "description": "Unique line tag to target for single-line operations, usually obtained from Read or Grep output."
                            },
                            "from_tag": {
                                "type": "string",
                                "description": "Inclusive start tag for `delete_range`, `overwrite_range`, or `regex_replace`. Also accepted for `move_range` as the range start tag."
                            },
                            "front_tag": {
                                "type": "string",
                                "description": "Inclusive start tag for `move_range`. Equivalent to `from_tag` for that operation."
                            },
                            "to_tag": {
                                "type": "string",
                                "description": "Inclusive end tag for `delete_range`, `move_range`, `overwrite_range`, or `regex_replace`. For `regex_replace`, if omitted only the `from_tag` line is modified."
                            },
                            "move_after_tag": {
                                "type": "string",
                                "description": "For `move_range`, move the selected inclusive range so it appears immediately after this tag. It must be outside the moved range."
                            },
                            "new_text": {
                                "type": "string",
                                "description": "Replacement content for `replace_line`. Provide one line of text without a trailing newline."
                            },
                            "new_lines": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "New line contents for `insert_before` or `insert_after`. Each string is one inserted line without a trailing newline."
                            },
                            "new_content": {
                                "type": "string",
                                "description": "Replacement content for `overwrite_range`. It may be empty, single-line, or multi-line. Extra inserted lines receive fresh tags."
                            },
                            "pattern": {
                                "type": "string",
                                "description": "For `regex_replace`, a non-empty Rust `regex` pattern applied to each selected line individually."
                            },
                            "replacement": {
                                "type": "string",
                                "description": "For `regex_replace`, the replacement string. It is required, may be empty, and must not produce multi-line output."
                            }
                        },
                        "required": ["op"]
                    }
                }
            },
            "required": ["file_path", "operations"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let req: XEditRequest = match serde_json::from_value(input) {
            Ok(req) => req,
            Err(err) => return ToolResult::error(format!("Invalid input: {}", err)),
        };

        let path = resolve_xfile_path(ctx, &req.file_path);
        let before_head = match ensure_loaded(&ctx.session_id, &path).await {
            Ok(head) => head,
            Err(err) => return ToolResult::error(err),
        };

        let operations = match req
            .operations
            .iter()
            .map(operation_to_mutation)
            .collect::<std::result::Result<Vec<_>, _>>()
        {
            Ok(ops) => ops,
            Err(err) => return ToolResult::error(err),
        };

        let head = match apply_mutations(
            &ctx.session_id,
            &path,
            req.base_version.as_deref(),
            &operations,
        ) {
            Ok(head) => head,
            Err(err) => return ToolResult::error(err),
        };

        if let Err(err) = tokio::fs::write(&path, &head.rendered_content).await {
            return ToolResult::error(format!("Failed to flush edited file: {}", err));
        }

        let payload = XEditSuccess {
            ok: true,
            file_path: path.display().to_string(),
            current_version: head.current_version.clone(),
            revision_count: head.revision_count,
            applied_operations: operations.len(),
            diff: make_tagged_diff(&before_head.file, &head.file),
        };

        ToolResult::success(serde_json::to_string_pretty(&payload).unwrap_or_default())
            .with_metadata(serde_json::json!({
                "file_path": payload.file_path,
                "current_version": payload.current_version,
                "revision_count": payload.revision_count,
                "applied_operations": payload.applied_operations,
                "diff": payload.diff,
            }))
    }
}

fn make_tagged_diff(old: &XFile, new: &XFile) -> String {
    let old_tags: std::collections::HashSet<&str> =
        old.content.iter().map(|line| line.tag.as_str()).collect();
    let new_tags: std::collections::HashSet<&str> =
        new.content.iter().map(|line| line.tag.as_str()).collect();

    let mut lines = vec![
        format!("--- {}", old.path.display()),
        format!("+++ {}", new.path.display()),
        "@@ tags @@".to_string(),
    ];

    let mut old_idx = 0usize;
    let mut new_idx = 0usize;

    while old_idx < old.content.len() || new_idx < new.content.len() {
        match (old.content.get(old_idx), new.content.get(new_idx)) {
            (Some(old_line), Some(new_line)) if old_line.tag == new_line.tag => {
                if old_line.content != new_line.content {
                    lines.push(format!("-{}\t{}", old_line.tag, old_line.content));
                    lines.push(format!("+{}\t{}", new_line.tag, new_line.content));
                }
                old_idx += 1;
                new_idx += 1;
            }
            (Some(old_line), Some(_)) if !new_tags.contains(old_line.tag.as_str()) => {
                lines.push(format!("-{}\t{}", old_line.tag, old_line.content));
                old_idx += 1;
            }
            (Some(_), Some(new_line)) if !old_tags.contains(new_line.tag.as_str()) => {
                lines.push(format!("+{}\t{}", new_line.tag, new_line.content));
                new_idx += 1;
            }
            (Some(old_line), Some(new_line)) => {
                lines.push(format!("-{}\t{}", old_line.tag, old_line.content));
                lines.push(format!("+{}\t{}", new_line.tag, new_line.content));
                old_idx += 1;
                new_idx += 1;
            }
            (Some(old_line), None) => {
                lines.push(format!("-{}\t{}", old_line.tag, old_line.content));
                old_idx += 1;
            }
            (None, Some(new_line)) => {
                lines.push(format!("+{}\t{}", new_line.tag, new_line.content));
                new_idx += 1;
            }
            (None, None) => break,
        }
    }

    if lines.len() == 3 {
        lines.push("(no textual changes)".to_string());
    }

    lines.join("\n")
}

fn operation_to_mutation(operation: &XEditOperation) -> std::result::Result<XLineMutation, String> {
    match operation.op.as_str() {
        "replace_line" => Ok(XLineMutation::ReplaceLine {
            tag: required_field(&operation.tag, "replace_line", "tag")?,
            new_text: operation
                .new_text
                .clone()
                .ok_or_else(|| "replace_line requires `new_text`.".to_string())?,
        }),
        "insert_before" => Ok(XLineMutation::InsertBefore {
            tag: required_field(&operation.tag, "insert_before", "tag")?,
            new_lines: operation.new_lines.clone().unwrap_or_default(),
        }),
        "insert_after" => Ok(XLineMutation::InsertAfter {
            tag: required_field(&operation.tag, "insert_after", "tag")?,
            new_lines: operation.new_lines.clone().unwrap_or_default(),
        }),
        "delete_line" => Ok(XLineMutation::DeleteLine {
            tag: required_field(&operation.tag, "delete_line", "tag")?,
        }),
        "delete_range" => Ok(XLineMutation::DeleteRange {
            from_tag: required_field(&operation.from_tag, "delete_range", "from_tag")?,
            to_tag: required_field(&operation.to_tag, "delete_range", "to_tag")?,
        }),
        "move_range" => Ok(XLineMutation::MoveRange {
            from_tag: operation
                .front_tag
                .clone()
                .or_else(|| operation.from_tag.clone())
                .ok_or_else(|| "move_range requires `front_tag` (or `from_tag`).".to_string())?,
            to_tag: required_field(&operation.to_tag, "move_range", "to_tag")?,
            move_after_tag: required_field(
                &operation.move_after_tag,
                "move_range",
                "move_after_tag",
            )?,
        }),
        "overwrite_range" => Ok(XLineMutation::OverwriteRange {
            from_tag: required_field(&operation.from_tag, "overwrite_range", "from_tag")?,
            to_tag: required_field(&operation.to_tag, "overwrite_range", "to_tag")?,
            new_content: operation
                .new_content
                .clone()
                .ok_or_else(|| "overwrite_range requires `new_content`.".to_string())?,
        }),
        "regex_replace" => {
            let pattern = required_field(&operation.pattern, "regex_replace", "pattern")?;
            if pattern.is_empty() {
                return Err("regex_replace requires non-empty `pattern`.".to_string());
            }
            Ok(XLineMutation::RegexReplace {
                from_tag: required_field(&operation.from_tag, "regex_replace", "from_tag")?,
                to_tag: operation
                    .to_tag
                    .clone()
                    .or_else(|| operation.from_tag.clone())
                    .ok_or_else(|| "regex_replace requires `from_tag`.".to_string())?,
                pattern,
                replacement: required_field(
                    &operation.replacement,
                    "regex_replace",
                    "replacement",
                )?,
            })
        }
        other => Err(format!("Unsupported Edit operation '{}'.", other)),
    }
}

fn required_field(
    value: &Option<String>,
    operation: &str,
    field: &str,
) -> std::result::Result<String, String> {
    value
        .clone()
        .ok_or_else(|| format!("{} requires `{}`.", operation, field))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::AllowAll;
    use crate::xfile_storage::{clear_session_xfile_storage, store_written_text, try_get_head};
    use std::path::Path;
    use std::sync::Arc;
    use uuid::Uuid;

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
    fn xedit_schema_exposes_tagged_operations() {
        let tool = XEditTool;
        let schema = tool.input_schema();

        assert_eq!(schema["properties"]["operations"]["type"], "array");
        assert_eq!(
            schema["properties"]["operations"]["items"]["properties"]["tag"]["type"],
            "string"
        );
        assert_eq!(
            schema["properties"]["operations"]["items"]["properties"]["from_tag"]["type"],
            "string"
        );
        assert_eq!(
            schema["properties"]["operations"]["items"]["properties"]["new_content"]["type"],
            "string"
        );
        assert_eq!(
            schema["properties"]["operations"]["items"]["properties"]["pattern"]["type"],
            "string"
        );
        assert_eq!(
            schema["properties"]["operations"]["items"]["properties"]["replacement"]["type"],
            "string"
        );
    }

    #[test]
    fn filesystem_toolset_includes_xedit() {
        let names: Vec<String> = crate::filesystem()
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect();

        assert!(names.iter().any(|name| name == "Edit"));
    }

    #[tokio::test]
    async fn xedit_replaces_inserts_and_flushes() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xedit-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("sample.txt");
        let initial = store_written_text(&session_id, &path, "alpha\nbeta\n");
        tokio::fs::write(&path, &initial.rendered_content)
            .await
            .unwrap();

        let first_tag = initial.file.content[0].tag.clone();
        let second_tag = initial.file.content[1].tag.clone();
        let tool = XEditTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "base_version": initial.current_version,
                    "operations": [
                        {
                            "op": "replace_line",
                            "tag": first_tag,
                            "new_text": "ALPHA"
                        },
                        {
                            "op": "insert_after",
                            "tag": second_tag,
                            "new_lines": ["gamma"]
                        }
                    ]
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        let disk = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(disk, "ALPHA\nbeta\ngamma\n");

        let head = try_get_head(&session_id, &path).unwrap();
        assert_eq!(head.revision_count, 2);
        assert_eq!(head.file.content[0].content, "ALPHA");
        assert_eq!(head.file.content[0].tag, initial.file.content[0].tag);
        assert_eq!(head.file.content[1].tag, initial.file.content[1].tag);
        assert_eq!(head.file.content[2].content, "gamma");

        let payload: XEditSuccess = serde_json::from_str(&result.content).unwrap();
        assert!(payload.diff.contains("---"));
        assert!(payload.diff.contains("+++"));
        assert!(payload.diff.contains("@@ tags @@"));
        assert!(payload
            .diff
            .contains(&format!("-{}\talpha", initial.file.content[0].tag)));
        assert!(payload
            .diff
            .contains(&format!("+{}\tALPHA", initial.file.content[0].tag)));
        assert!(payload
            .diff
            .contains(&format!("+{}\tgamma", head.file.content[2].tag)));
    }

    #[tokio::test]
    async fn xedit_supports_range_operations_and_aliases() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xedit-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("range.txt");
        let initial = store_written_text(&session_id, &path, "one\ntwo\nthree\nfour\n");
        tokio::fs::write(&path, &initial.rendered_content)
            .await
            .unwrap();

        let first_tag = initial.file.content[0].tag.clone();
        let second_tag = initial.file.content[1].tag.clone();
        let third_tag = initial.file.content[2].tag.clone();
        let fourth_tag = initial.file.content[3].tag.clone();

        let tool = XEditTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "base_version": initial.current_version,
                    "operations": [
                        {
                            "op": "move_range",
                            "front_tag": second_tag,
                            "to_tage": third_tag,
                            "move_after_tag": fourth_tag
                        },
                        {
                            "op": "overwrite_range",
                            "from_tag": first_tag,
                            "to_tag": first_tag,
                            "new_content": "ONE\nONE-POINT-FIVE"
                        },
                        {
                            "op": "delete_range",
                            "from_tag": fourth_tag,
                            "to_tag": fourth_tag
                        }
                    ]
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        let disk = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(disk, "ONE\nONE-POINT-FIVE\ntwo\nthree\n");

        let head = try_get_head(&session_id, &path).unwrap();
        assert_eq!(head.revision_count, 2);
        assert_eq!(head.file.content[0].tag, initial.file.content[0].tag);
        assert_eq!(head.file.content[2].tag, initial.file.content[1].tag);
        assert_eq!(head.file.content[3].tag, initial.file.content[2].tag);
        assert_ne!(head.file.content[1].tag, initial.file.content[0].tag);
        assert!(!head
            .file
            .content
            .iter()
            .any(|line| line.tag == initial.file.content[3].tag));

        let payload: XEditSuccess = serde_json::from_str(&result.content).unwrap();
        assert!(payload
            .diff
            .contains(&format!("-{}\tfour", initial.file.content[3].tag)));
    }

    #[tokio::test]
    async fn xedit_regex_replace_preserves_tags_and_edits_lines_individually() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xedit-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("regex.txt");
        let initial = store_written_text(&session_id, &path, "a1\nb2\nc3\n");
        tokio::fs::write(&path, &initial.rendered_content)
            .await
            .unwrap();

        let first_tag = initial.file.content[0].tag.clone();
        let second_tag = initial.file.content[1].tag.clone();
        let third_tag = initial.file.content[2].tag.clone();

        let tool = XEditTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "base_version": initial.current_version,
                    "operations": [
                        {
                            "op": "regex_replace",
                            "from_tag": first_tag,
                            "to_tag": second_tag,
                            "pattern": "\\d",
                            "replacement": "X"
                        },
                        {
                            "op": "regex_replace",
                            "from_tag": third_tag,
                            "pattern": "c",
                            "replacement": "C"
                        }
                    ]
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        let disk = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(disk, "aX\nbX\nC3\n");

        let head = try_get_head(&session_id, &path).unwrap();
        assert_eq!(head.file.content[0].tag, initial.file.content[0].tag);
        assert_eq!(head.file.content[1].tag, initial.file.content[1].tag);
        assert_eq!(head.file.content[2].tag, initial.file.content[2].tag);
        assert_eq!(head.file.content.len(), 3);
    }

    #[tokio::test]
    async fn xedit_regex_replace_rejects_empty_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xedit-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("regex-empty.txt");
        let initial = store_written_text(&session_id, &path, "a1\n");
        tokio::fs::write(&path, &initial.rendered_content)
            .await
            .unwrap();

        let tool = XEditTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "base_version": initial.current_version,
                    "operations": [
                        {
                            "op": "regex_replace",
                            "from_tag": initial.file.content[0].tag,
                            "pattern": "",
                            "replacement": "X"
                        }
                    ]
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(result.is_error);
        assert_eq!(
            result.content,
            "regex_replace requires non-empty `pattern`."
        );
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "a1\n");
    }

    #[tokio::test]
    async fn xedit_regex_replace_rejects_multiline_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xedit-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("regex-multiline.txt");
        let initial = store_written_text(&session_id, &path, "a1\n");
        tokio::fs::write(&path, &initial.rendered_content)
            .await
            .unwrap();

        let tool = XEditTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "base_version": initial.current_version,
                    "operations": [
                        {
                            "op": "regex_replace",
                            "from_tag": initial.file.content[0].tag,
                            "pattern": "1",
                            "replacement": "1\n2"
                        }
                    ]
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(result.is_error);
        assert!(result
            .content
            .contains("must not create multi-line content"));
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "a1\n");

        let head = try_get_head(&session_id, &path).unwrap();
        assert_eq!(head.revision_count, 1);
        assert_eq!(head.rendered_content, "a1\n");
    }

    #[tokio::test]
    async fn xedit_move_range_rejects_move_after_tag_inside_range() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = format!("xedit-test-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);
        let path = tmp.path().join("move-invalid.txt");
        let initial = store_written_text(&session_id, &path, "one\ntwo\nthree\n");
        tokio::fs::write(&path, &initial.rendered_content)
            .await
            .unwrap();

        let tool = XEditTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "base_version": initial.current_version,
                    "operations": [
                        {
                            "op": "move_range",
                            "front_tag": initial.file.content[0].tag,
                            "to_tag": initial.file.content[1].tag,
                            "move_after_tag": initial.file.content[1].tag
                        }
                    ]
                }),
                &test_ctx(tmp.path(), &session_id),
            )
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("must be outside the moved range"));
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "one\ntwo\nthree\n"
        );
    }
}
