//! PDF text extraction tool.

use super::*;
use pdf_extract::extract_text;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tokio::task;

const DEFAULT_MAX_CHARS: usize = 50_000;
const MAX_CHARS_LIMIT: usize = 200_000;
const MAX_PDF_FILE_SIZE_BYTES: u64 = 20 * 1024 * 1024;
const SEARCH_CONTEXT_LINES: usize = 2;

type StringResult<T> = std::result::Result<T, String>;

pub struct PdfReadTool;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PdfReadRequest {
    pub file_path: String,
    #[serde(default)]
    pub start: Option<usize>,
    #[serde(default)]
    pub end: Option<usize>,
    #[serde(default)]
    pub length: Option<usize>,
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub before: Option<usize>,
    #[serde(default)]
    pub after: Option<usize>,
    #[serde(default)]
    pub max_chars: Option<usize>,
}

#[async_trait]
impl Tool for PdfReadTool {
    fn name(&self) -> &str {
        "PdfRead"
    }

    fn description(&self) -> &str {
        "Read text from a PDF file with bounded, agent-friendly output. Supports text-based PDFs; scanned PDFs may require OCR and may not yield useful text."
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
                "file_path": {
                    "type": "string",
                    "description": "Path to the PDF file. Absolute paths and workspace-relative paths are accepted."
                },
                "start": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Optional starting character offset in the extracted PDF text. Defaults to 0."
                },
                "end": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Optional exclusive ending character offset in the extracted PDF text. Use with `start`."
                },
                "length": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional maximum number of characters to return from the selected range."
                },
                "search": {
                    "type": "string",
                    "description": "Optional Rust `regex` pattern. If set and non-empty, PdfRead filters extracted text to matching lines plus nearby context, separating match groups with `---------------`."
                },
                "before": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Optional number of context lines to include before each search match."
                },
                "after": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Optional number of context lines to include after each search match."
                },

                "max_chars": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional maximum number of characters to extract from the PDF output after processing. Default 50000. Hard-capped for safety."
                }
            },
            "required": ["file_path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let req: PdfReadRequest = match serde_json::from_value(input) {
            Ok(req) => req,
            Err(err) => return ToolResult::error(format!("Invalid input: {}", err)),
        };

        let path = match resolve_pdf_path(ctx, &req.file_path) {
            Ok(path) => path,
            Err(err) => return ToolResult::error(err),
        };

        let extraction_path = path.clone();
        let extracted = match task::spawn_blocking(move || extract_text(&extraction_path)).await {
            Ok(Ok(text)) => text,
            Ok(Err(err)) => {
                return ToolResult::error(format!(
                    "Failed to extract text from PDF '{}': {}",
                    path.display(),
                    err
                ))
            }
            Err(err) => {
                return ToolResult::error(format!(
                    "PDF extraction task failed for '{}': {}",
                    path.display(),
                    err
                ))
            }
        };

        let normalized = normalize_pdf_text(&extracted);
        let selected = match select_text_range(&normalized, req.start, req.end, req.length) {
            Ok(text) => text,
            Err(err) => return ToolResult::error(err),
        };
        let selected_chars = selected.chars().count();

        let search = req
            .search
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let mut output = if let Some(pattern) = search {
            let regex = match Regex::new(pattern) {
                Ok(regex) => regex,
                Err(err) => return ToolResult::error(format!("Invalid regex: {}", err)),
            };
            render_search_matches(
                &selected,
                &regex,
                req.before.unwrap_or(SEARCH_CONTEXT_LINES),
                req.after.unwrap_or(SEARCH_CONTEXT_LINES),
            )
        } else {
            selected.clone()
        };

        if output.is_empty() {
            output = if search.is_some() {
                "No matches found.".to_string()
            } else {
                "No extractable text found in PDF.".to_string()
            };
        }

        let limit = req
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .min(MAX_CHARS_LIMIT);
        let output_chars = output.chars().count();
        let truncated = truncate_chars(&output, limit);
        let was_truncated = limit < output_chars;

        let final_output = if was_truncated {
            format!(
                "{}\n\nOutput truncated to {} characters. Refine `start`, `end`, `length`, or `search` to inspect a narrower slice.",
                truncated, limit
            )
        } else {
            truncated
        };
        let final_output_chars = final_output.chars().count();

        ToolResult::success(final_output).with_metadata(serde_json::json!({
            "file_path": path.display().to_string(),
            "extracted_chars": normalized.chars().count(),
            "selected_chars": selected_chars,
            "returned_chars": final_output_chars,
            "max_chars": limit,
            "truncated": was_truncated,
        }))
    }
}

pub fn is_pdf_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false)
}

fn resolve_pdf_path(ctx: &ToolContext, file_path: &str) -> StringResult<PathBuf> {
    let candidate = PathBuf::from(file_path);
    let path = if candidate.is_absolute() {
        candidate
    } else {
        ctx.working_dir.join(candidate)
    };

    let canonical = path
        .canonicalize()
        .map_err(|e| format!("Cannot access PDF '{}': {}", path.display(), e))?;

    let workspace = ctx
        .working_dir
        .canonicalize()
        .unwrap_or_else(|_| ctx.working_dir.clone());
    if !canonical.starts_with(&workspace) {
        return Err("PDF access is only allowed within the workspace root.".to_string());
    }

    let metadata = fs::metadata(&canonical)
        .map_err(|e| format!("Cannot stat PDF '{}': {}", canonical.display(), e))?;
    if !metadata.is_file() {
        return Err(format!("'{}' is not a file.", canonical.display()));
    }
    if metadata.len() > MAX_PDF_FILE_SIZE_BYTES {
        return Err(format!(
            "PDF '{}' is too large ({} bytes). Maximum supported size is {} bytes.",
            canonical.display(),
            metadata.len(),
            MAX_PDF_FILE_SIZE_BYTES
        ));
    }
    if !is_pdf_path(&canonical) {
        return Err(format!("'{}' is not a PDF file.", canonical.display()));
    }

    Ok(canonical)
}

fn normalize_pdf_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .to_string()
}

fn select_text_range(
    text: &str,
    start: Option<usize>,
    end: Option<usize>,
    length: Option<usize>,
) -> StringResult<String> {
    if end.is_some() && length.is_some() {
        return Err("PdfRead accepts either `end` or `length`, not both.".to_string());
    }

    let total_chars = text.chars().count();
    let start = start.unwrap_or(0);
    if start > total_chars {
        return Err(format!(
            "Start offset {} is beyond extracted text length {}.",
            start, total_chars
        ));
    }

    let end = if let Some(end) = end {
        if end < start {
            return Err(format!(
                "End offset {} must be greater than or equal to start {}.",
                end, start
            ));
        }
        end.min(total_chars)
    } else if let Some(length) = length {
        start.saturating_add(length).min(total_chars)
    } else {
        total_chars
    };

    let start_byte = char_to_byte_index(text, start);
    let end_byte = char_to_byte_index(text, end);
    Ok(text[start_byte..end_byte].to_string())
}

fn char_to_byte_index(text: &str, char_offset: usize) -> usize {
    if char_offset == 0 {
        return 0;
    }

    text.char_indices()
        .map(|(idx, _)| idx)
        .nth(char_offset)
        .unwrap_or(text.len())
}

fn render_search_matches(text: &str, regex: &Regex, before: usize, after: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut windows = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        if regex.is_match(line) {
            let start = idx.saturating_sub(before);
            let end = (idx + after + 1).min(lines.len());
            windows.push((start, end));
        }
    }

    if windows.is_empty() {
        return String::new();
    }

    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in windows {
        if let Some((_, prev_end)) = merged.last_mut() {
            if start <= *prev_end {
                *prev_end = (*prev_end).max(end);
                continue;
            }
        }
        merged.push((start, end));
    }

    let mut chunks = Vec::new();
    for (idx, (start, end)) in merged.into_iter().enumerate() {
        if idx > 0 {
            chunks.push("---------------".to_string());
        }
        for line in &lines[start..end] {
            chunks.push((*line).to_string());
        }
    }

    chunks.join("\n")
}

fn truncate_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::AllowAll;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn test_ctx(working_dir: PathBuf) -> ToolContext {
        ToolContext {
            session_id: format!("pdf-test-{}", uuid::Uuid::new_v4()),
            working_dir,
            permissions: Arc::new(AllowAll),
            ..ToolContext::default()
        }
    }

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[tokio::test]
    async fn pdf_read_extracts_fixture_text() {
        let tool = PdfReadTool;
        let path = fixture_path("pdf.pdf");
        let ctx = test_ctx(PathBuf::from(env!("CARGO_MANIFEST_DIR")));

        let result = tool
            .execute(json!({ "file_path": path.display().to_string() }), &ctx)
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("one"), "{}", result.content);
        let metadata = result.metadata.expect("metadata");
        assert_eq!(metadata["truncated"], false);
        assert!(metadata["returned_chars"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn pdf_read_supports_search_and_range_controls() {
        let tool = PdfReadTool;
        let path = fixture_path("pdf.pdf");
        let ctx = test_ctx(PathBuf::from(env!("CARGO_MANIFEST_DIR")));

        let result = tool
            .execute(
                json!({
                    "file_path": path.display().to_string(),
                    "search": "one|two",
                    "start": 0,
                    "length": 200,
                    "max_chars": 5000
                }),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("one"), "{}", result.content);
        assert!(!result.content.contains("No matches found."));
        let metadata = result.metadata.expect("metadata");
        assert!(metadata["selected_chars"].as_u64().unwrap() <= 200);
    }

    #[tokio::test]
    async fn pdf_read_search_respects_before_and_after() {
        let tool = PdfReadTool;
        let path = fixture_path("pdf.pdf");
        let ctx = test_ctx(PathBuf::from(env!("CARGO_MANIFEST_DIR")));

        let with_context = tool
            .execute(
                json!({
                    "file_path": path.display().to_string(),
                    "search": "two",
                    "before": 1,
                    "after": 1,
                    "max_chars": 5000
                }),
                &ctx,
            )
            .await;

        assert!(!with_context.is_error, "{}", with_context.content);
        assert!(
            with_context.content.contains("one"),
            "{}",
            with_context.content
        );
        assert!(
            with_context.content.contains("two"),
            "{}",
            with_context.content
        );
        assert!(
            with_context.content.contains("three"),
            "{}",
            with_context.content
        );

        let without_context = tool
            .execute(
                json!({
                    "file_path": path.display().to_string(),
                    "search": "two",
                    "before": 0,
                    "after": 0,
                    "max_chars": 5000
                }),
                &ctx,
            )
            .await;

        assert!(!without_context.is_error, "{}", without_context.content);
        assert_eq!(without_context.content.trim(), "two");
    }

    #[test]
    fn select_text_range_uses_character_offsets() {
        let selected = select_text_range("héllo", Some(1), None, Some(3)).expect("slice");
        assert_eq!(selected, "éll");
    }
}
