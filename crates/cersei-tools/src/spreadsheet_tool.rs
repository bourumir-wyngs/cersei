//! Spreadsheet tools for Excel/OpenDocument spreadsheet support.

use super::*;
use calamine::{open_workbook_auto, Reader, Sheets};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_ROW_LIMIT: usize = 20;
type StringResult<T> = std::result::Result<T, String>;
const MAX_ROW_LIMIT: usize = 200;
const MAX_COLUMN_LIMIT: usize = 64;

pub struct SpreadsheetInfoTool;
pub struct SpreadsheetReadTool;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpreadsheetInfoRequest {
    pub file_path: String,
    #[serde(default)]
    pub include_ranges: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpreadsheetReadRequest {
    pub file_path: String,
    #[serde(default)]
    pub sheet_name: Option<String>,
    #[serde(default)]
    pub range: Option<String>,
    #[serde(default)]
    pub start_row: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub start_col: Option<usize>,
    #[serde(default)]
    pub col_limit: Option<usize>,
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SheetSummary {
    name: String,
    total_rows: usize,
    total_columns: usize,
    non_empty_cells: usize,
    used_range: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct Selection {
    start_row: usize,
    end_row: usize,
    start_col: usize,
    end_col: usize,
    truncated_rows: bool,
    truncated_columns: bool,
    requested_start_row: usize,
    requested_end_row: usize,
    requested_start_col: usize,
    requested_end_col: usize,
}

#[async_trait]
impl Tool for SpreadsheetInfoTool {
    fn name(&self) -> &str {
        "SpreadsheetInfo"
    }

    fn description(&self) -> &str {
        "Inspect a spreadsheet file and return basic metadata such as available sheet names and dimensions. Supports Excel and OpenDocument spreadsheet files."
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
                    "description": "Path to the spreadsheet file. Absolute paths and workspace-relative paths are accepted."
                },
                "include_ranges": {
                    "type": "boolean",
                    "description": "Whether to include per-sheet used-range metadata."
                }
            },
            "required": ["file_path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let req: SpreadsheetInfoRequest = match serde_json::from_value(input) {
            Ok(req) => req,
            Err(err) => return ToolResult::error(format!("Invalid input: {}", err)),
        };

        let path = match resolve_spreadsheet_path(ctx, &req.file_path) {
            Ok(path) => path,
            Err(err) => return ToolResult::error(err),
        };

        let workbook = match open_workbook(&path) {
            Ok(workbook) => workbook,
            Err(err) => return ToolResult::error(err),
        };

        let include_ranges = req.include_ranges.unwrap_or(false);
        let sheets = match collect_sheet_summaries(workbook, include_ranges) {
            Ok(sheets) => sheets,
            Err(err) => return ToolResult::error(err),
        };

        let mut output = format!("Spreadsheet: {}\n", path.display());
        if sheets.is_empty() {
            output.push_str("No sheets found.");
        } else {
            output.push_str(&format!("Sheets: {}\n\n", sheets.len()));
            for (index, sheet) in sheets.iter().enumerate() {
                if index > 0 {
                    output.push('\n');
                }
                output.push_str(&format!(
                    "- {}: {} rows x {} columns, {} non-empty cells",
                    sheet.name, sheet.total_rows, sheet.total_columns, sheet.non_empty_cells
                ));
                if let Some(range) = &sheet.used_range {
                    output.push_str(&format!(", range {}", range));
                }
            }
        }

        ToolResult::success(output).with_metadata(serde_json::json!({
            "file_path": path.display().to_string(),
            "sheet_count": sheets.len(),
            "sheets": sheets,
        }))
    }
}

#[async_trait]
impl Tool for SpreadsheetReadTool {
    fn name(&self) -> &str {
        "SpreadsheetRead"
    }

    fn description(&self) -> &str {
        "Read a sheet or range from a spreadsheet file with bounded, agent-friendly output. Supports Excel and OpenDocument spreadsheet files."
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
                    "description": "Path to the spreadsheet file. Absolute paths and workspace-relative paths are accepted."
                },
                "sheet_name": {
                    "type": "string",
                    "description": "Optional sheet name. Defaults to the first sheet."
                },
                "range": {
                    "type": "string",
                    "description": "Optional spreadsheet-style range, for example 'A1:D20'."
                },
                "start_row": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Optional 0-based starting row for paginated reads. Ignored when `range` is provided."
                },
                "start_col": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Optional 0-based starting column for paginated reads. Ignored when `range` is provided unless combined with an explicit column slice override."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of rows to return. Default 20. Hard-capped for safety."
                },
                "col_limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of columns to return. Default 64. Hard-capped for safety."
                },
                "format": {
                    "type": "string",
                    "description": "Preferred output format. Currently supports 'markdown' and 'csv'; defaults to 'markdown'."
                }
            },
            "required": ["file_path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let req: SpreadsheetReadRequest = match serde_json::from_value(input) {
            Ok(req) => req,
            Err(err) => return ToolResult::error(format!("Invalid input: {}", err)),
        };

        let path = match resolve_spreadsheet_path(ctx, &req.file_path) {
            Ok(path) => path,
            Err(err) => return ToolResult::error(err),
        };

        let mut workbook = match open_workbook(&path) {
            Ok(workbook) => workbook,
            Err(err) => return ToolResult::error(err),
        };

        let sheet_names = workbook.sheet_names().to_vec();
        if sheet_names.is_empty() {
            return ToolResult::error("Spreadsheet contains no sheets.");
        }

        let sheet_name = match req.sheet_name {
            Some(name) => {
                if sheet_names.iter().any(|candidate| candidate == &name) {
                    name
                } else {
                    return ToolResult::error(format!(
                        "Sheet '{}' was not found. Available sheets: {}",
                        name,
                        sheet_names.join(", ")
                    ));
                }
            }
            None => sheet_names[0].clone(),
        };

        let range = match worksheet_range(&mut workbook, &sheet_name) {
            Ok(range) => range,
            Err(err) => return ToolResult::error(err),
        };

        let total_rows = range.height();
        let total_columns = range.width();
        let limit = req.limit.unwrap_or(DEFAULT_ROW_LIMIT).min(MAX_ROW_LIMIT);
        let format = req.format.as_deref().unwrap_or("markdown");

        let selection = match build_selection(&range, req.range.as_deref(), req.start_row, req.start_col, limit, req.col_limit.unwrap_or(MAX_COLUMN_LIMIT).min(MAX_COLUMN_LIMIT)) {
            Ok(selection) => selection,
            Err(err) => return ToolResult::error(err),
        };

        if total_rows == 0
            || total_columns == 0
            || selection.start_row >= selection.end_row
            || selection.start_col >= selection.end_col
        {
            return ToolResult::success(format!(
                "Sheet '{}' is empty for the requested selection.",
                sheet_name
            ))
            .with_metadata(serde_json::json!({
                "file_path": path.display().to_string(),
                "sheet_name": sheet_name,
                "total_rows": total_rows,
                "total_columns": total_columns,
                "selected_rows": 0,
                "selected_range": req.range,
            }));
        }

        let sheet_start = range.start().unwrap_or((0, 0));
        let rel_start_row = selection.start_row.saturating_sub(sheet_start.0 as usize);
        let rel_end_row = selection.end_row.saturating_sub(sheet_start.0 as usize);
        let rel_start_col = selection.start_col.saturating_sub(sheet_start.1 as usize);
        let rel_end_col = selection.end_col.saturating_sub(sheet_start.1 as usize);

        let rows: Vec<Vec<String>> = range
            .rows()
            .skip(rel_start_row)
            .take(rel_end_row.saturating_sub(rel_start_row))
            .map(|row| {
                row.iter()
                    .skip(rel_start_col)
                    .take(rel_end_col.saturating_sub(rel_start_col))
                    .map(cell_to_string)
                    .collect()
            })
            .collect();

        let rendered = match format {
            "markdown" => render_markdown_table(&rows),
            "csv" => render_csv(&rows),
            other => {
                return ToolResult::error(format!(
                    "Unsupported format '{}'. Supported formats: markdown, csv.",
                    other
                ))
            }
        };

        let mut output = format!(
            "Spreadsheet: {}\nSheet: {}\nRows: {}..{} of {}\nColumns: {}..{} of {}\nRequested rows: {}..{}\nRequested columns: {}..{}\nFormat: {}\n\n{}",
            path.display(),
            sheet_name,
            selection.start_row,
            selection.end_row,
            total_rows,
            selection.start_col,
            selection.end_col,
            total_columns,
            selection.requested_start_row,
            selection.requested_end_row,
            selection.requested_start_col,
            selection.requested_end_col,
            format,
            rendered
        );

        if selection.truncated_rows || selection.truncated_columns {
            output.push_str("\n\n[truncated");
            if selection.truncated_rows {
                output.push_str(&format!(" rows {}..{}", selection.start_row, selection.end_row));
            }
            if selection.truncated_columns {
                if selection.truncated_rows {
                    output.push(',');
                }
                output.push_str(&format!(" columns {}..{}", selection.start_col, selection.end_col));
            }
            output.push(']');
        }

        ToolResult::success(output).with_metadata(serde_json::json!({
            "file_path": path.display().to_string(),
            "sheet_name": sheet_name,
            "total_rows": total_rows,
            "total_columns": total_columns,
            "start_row": selection.start_row,
            "end_row": selection.end_row,
            "start_col": selection.start_col,
            "end_col": selection.end_col,
            "requested_start_row": selection.requested_start_row,
            "requested_end_row": selection.requested_end_row,
            "requested_start_col": selection.requested_start_col,
            "requested_end_col": selection.requested_end_col,
            "selected_rows": rows.len(),
            "selected_range": req.range,
            "format": format,
            "truncated_rows": selection.truncated_rows,
            "truncated_columns": selection.truncated_columns,
        }))
    }
}

fn resolve_spreadsheet_path(ctx: &ToolContext, file_path: &str) -> StringResult<PathBuf> {
    let candidate = PathBuf::from(file_path);
    let path = if candidate.is_absolute() {
        candidate
    } else {
        ctx.working_dir.join(candidate)
    };

    let canonical = path
        .canonicalize()
        .map_err(|e| format!("Cannot access spreadsheet '{}': {}", path.display(), e))?;

    let workspace = ctx
        .working_dir
        .canonicalize()
        .unwrap_or_else(|_| ctx.working_dir.clone());
    if !canonical.starts_with(&workspace) {
        return Err("Spreadsheet access is only allowed within the workspace root.".to_string());
    }

    let metadata = fs::metadata(&canonical)
        .map_err(|e| format!("Cannot stat spreadsheet '{}': {}", canonical.display(), e))?;
    if !metadata.is_file() {
        return Err(format!("'{}' is not a file.", canonical.display()));
    }

    Ok(canonical)
}

fn open_workbook(path: &Path) -> StringResult<Sheets<std::io::BufReader<fs::File>>> {
    open_workbook_auto(path)
        .map_err(|e| format!("Failed to open spreadsheet '{}': {}", path.display(), e))
}

fn worksheet_range(
    workbook: &mut Sheets<std::io::BufReader<fs::File>>,
    sheet_name: &str,
) -> StringResult<calamine::Range<calamine::Data>> {
    workbook
        .worksheet_range(sheet_name)
        .map_err(|e| format!("Failed to read sheet '{}': {}", sheet_name, e))
}

fn collect_sheet_summaries(
    mut workbook: Sheets<std::io::BufReader<fs::File>>,
    include_ranges: bool,
) -> StringResult<Vec<SheetSummary>> {
    let names = workbook.sheet_names().to_vec();
    let mut sheets = Vec::with_capacity(names.len());

    for name in names {
        let range = worksheet_range(&mut workbook, &name)?;
        let start = range.start().unwrap_or((0, 0));
        let used_range = if include_ranges && range.width() > 0 && range.height() > 0 {
            Some(format!(
                "{}{}:{}{}",
                column_label(start.1 as usize),
                start.0 as usize + 1,
                column_label(start.1 as usize + range.width() - 1),
                start.0 as usize + range.height()
            ))
        } else {
            None
        };

        let non_empty_cells = range
            .rows()
            .map(|row| row.iter().filter(|cell| !cell_to_string(cell).is_empty()).count())
            .sum();

        sheets.push(SheetSummary {
            name,
            total_rows: range.height(),
            total_columns: range.width(),
            non_empty_cells,
            used_range,
        });
    }

    Ok(sheets)
}

fn build_selection(
    range: &calamine::Range<calamine::Data>,
    requested_range: Option<&str>,
    start_row: Option<usize>,
    start_col: Option<usize>,
    row_limit: usize,
    col_limit: usize,
) -> StringResult<Selection> {
    let sheet_start = range.start().unwrap_or((0, 0));
    let sheet_start_row = sheet_start.0 as usize;
    let sheet_start_col = sheet_start.1 as usize;
    let total_rows = range.height();
    let total_columns = range.width();
    let sheet_end_row = sheet_start_row + total_rows;
    let sheet_end_col = sheet_start_col + total_columns;

    if total_rows == 0 || total_columns == 0 {
        return Ok(Selection {
            start_row: sheet_start_row,
            end_row: sheet_start_row,
            start_col: sheet_start_col,
            end_col: sheet_start_col,
            requested_start_row: sheet_start_row,
            requested_end_row: sheet_start_row,
            requested_start_col: sheet_start_col,
            requested_end_col: sheet_start_col,
            truncated_rows: false,
            truncated_columns: false,
        });
    }

    match requested_range {
        Some(text) => {
            let mut bounds = parse_a1_range(text)?;
            bounds.requested_start_row = bounds.start_row;
            bounds.requested_end_row = bounds.end_row;
            bounds.requested_start_col = bounds.start_col;
            bounds.requested_end_col = bounds.end_col;

            if let Some(explicit_start_col) = start_col {
                let requested_width = col_limit;
                bounds.requested_start_col = explicit_start_col;
                bounds.requested_end_col = explicit_start_col.saturating_add(requested_width);
                bounds.start_col = explicit_start_col;
                bounds.end_col = explicit_start_col.saturating_add(requested_width);
            }

            bounds.start_row = bounds.start_row.max(sheet_start_row);
            bounds.start_col = bounds.start_col.max(sheet_start_col);
            bounds.end_row = bounds.end_row.min(sheet_end_row);
            bounds.end_col = bounds.end_col.min(sheet_end_col);

            if bounds.start_row >= bounds.end_row || bounds.start_col >= bounds.end_col {
                return Err(format!(
                    "Requested range '{}' is empty or outside the sheet's used range.",
                    text
                ));
            }

            let unclamped_end_row = bounds.end_row;
            let unclamped_end_col = bounds.end_col;
            bounds.end_row = bounds.start_row.saturating_add(row_limit).min(bounds.end_row);
            bounds.end_col = bounds.start_col.saturating_add(col_limit).min(bounds.end_col);
            bounds.truncated_rows = bounds.end_row < unclamped_end_row;
            bounds.truncated_columns = bounds.end_col < unclamped_end_col;
            Ok(bounds)
        }
        None => {
            let start_row = start_row.unwrap_or(sheet_start_row).max(sheet_start_row);
            let start_col = start_col.unwrap_or(sheet_start_col).max(sheet_start_col);
            if start_row >= sheet_end_row {
                return Err(format!(
                    "start_row {} is out of bounds for sheet used range starting at row {} with {} rows.",
                    start_row, sheet_start_row, total_rows
                ));
            }
            if start_col >= sheet_end_col {
                return Err(format!(
                    "start_col {} is out of bounds for sheet used range starting at column {} with {} columns.",
                    start_col, sheet_start_col, total_columns
                ));
            }
            Ok(Selection {
                start_row,
                end_row: start_row.saturating_add(row_limit).min(sheet_end_row),
                start_col,
                end_col: start_col.saturating_add(col_limit).min(sheet_end_col),
                requested_start_row: start_row,
                requested_end_row: start_row.saturating_add(row_limit),
                requested_start_col: start_col,
                requested_end_col: start_col.saturating_add(col_limit),
                truncated_rows: start_row.saturating_add(row_limit) < sheet_end_row,
                truncated_columns: start_col.saturating_add(col_limit) < sheet_end_col,
            })
        }
    }
}

fn parse_a1_range(range_text: &str) -> StringResult<Selection> {
    let trimmed = range_text.trim();
    let mut parts = trimmed.split(':');
    let start = parts
        .next()
        .ok_or_else(|| format!("Invalid range '{}'.", trimmed))?;
    let end = parts.next().unwrap_or(start);
    if parts.next().is_some() {
        return Err(format!("Invalid range '{}'.", trimmed));
    }

    let (start_col, start_row) = parse_a1_cell(start)?;
    let (end_col_inclusive, end_row_inclusive) = parse_a1_cell(end)?;
    let end_row = end_row_inclusive.saturating_add(1);
    let end_col = end_col_inclusive.saturating_add(1);

    if start_row >= end_row || start_col >= end_col {
        return Err(format!("Requested range '{}' is empty or inverted.", trimmed));
    }

    Ok(Selection {
        start_row,
        end_row,
        start_col,
        end_col,
        requested_start_row: start_row,
        requested_end_row: end_row,
        requested_start_col: start_col,
        requested_end_col: end_col,
        truncated_rows: false,
        truncated_columns: false,
    })
}

fn cell_to_string<T: std::fmt::Display>(cell: &T) -> String {
    cell.to_string()
}

fn render_markdown_table(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return "(no rows)".to_string();
    }

    let width = rows.iter().map(|row| row.len()).max().unwrap_or(0);
    if width == 0 {
        return "(no columns)".to_string();
    }

    let normalized: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            let mut padded = row.clone();
            padded.resize(width, String::new());
            padded
        })
        .collect();

    let mut out = String::new();
    out.push('|');
    for cell in &normalized[0] {
        out.push(' ');
        out.push_str(&escape_markdown_cell(cell));
        out.push(' ');
        out.push('|');
    }
    out.push('\n');

    out.push('|');
    for _ in 0..width {
        out.push_str(" --- |");
    }
    out.push('\n');

    for row in normalized.iter().skip(1) {
        out.push('|');
        for cell in row {
            out.push(' ');
            out.push_str(&escape_markdown_cell(cell));
            out.push(' ');
            out.push('|');
        }
        out.push('\n');
    }

    out.trim_end().to_string()
}

fn render_csv(rows: &[Vec<String>]) -> String {
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|cell| escape_csv_cell(cell))
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn escape_markdown_cell(cell: &str) -> String {
    cell.replace('|', "\\|").replace('\n', "<br>")
}

fn escape_csv_cell(cell: &str) -> String {
    let escaped = cell.replace('"', "\"\"");
    if escaped.contains(',') || escaped.contains('\n') || escaped.contains('"') {
        format!("\"{}\"", escaped)
    } else {
        escaped
    }
}

fn parse_a1_cell(cell: &str) -> StringResult<(usize, usize)> {
    let trimmed = cell.trim();
    if trimmed.is_empty() {
        return Err("Cell reference must not be empty.".to_string());
    }

    let mut letters = String::new();
    let mut digits = String::new();
    for ch in trimmed.chars() {
        if ch.is_ascii_alphabetic() && digits.is_empty() {
            letters.push(ch.to_ascii_uppercase());
        } else if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            return Err(format!("Invalid cell reference '{}'.", trimmed));
        }
    }

    if letters.is_empty() || digits.is_empty() {
        return Err(format!("Invalid cell reference '{}'.", trimmed));
    }

    let col = letters.chars().fold(0usize, |acc, ch| {
        acc * 26 + ((ch as u8 - b'A') as usize + 1)
    }) - 1;
    let row = digits
        .parse::<usize>()
        .map_err(|_| format!("Invalid row number in '{}'.", trimmed))?
        .checked_sub(1)
        .ok_or_else(|| format!("Row number must start at 1 in '{}'.", trimmed))?;

    Ok((col, row))
}

fn column_label(mut index: usize) -> String {
    let mut label = String::new();
    loop {
        let rem = index % 26;
        label.insert(0, (b'A' + rem as u8) as char);
        if index < 26 {
            break;
        }
        index = (index / 26) - 1;
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;
    use calamine::{Cell, Data, Range};

    #[test]
    fn parses_multi_cell_range() {
        let selection = parse_a1_range("A2:D5").unwrap();
        assert_eq!(selection.start_row, 1);
        assert_eq!(selection.end_row, 5);
        assert_eq!(selection.start_col, 0);
        assert_eq!(selection.end_col, 4);
    }

    #[test]
    fn column_labels_are_generated() {
        assert_eq!(column_label(0), "A");
        assert_eq!(column_label(25), "Z");
        assert_eq!(column_label(26), "AA");
        assert_eq!(column_label(27), "AB");
    }

    #[test]
    fn build_selection_respects_offset_and_limits() {
        let cells = vec![
            Cell::new((2, 2), Data::String("h1".into())),
            Cell::new((2, 3), Data::String("h2".into())),
            Cell::new((3, 2), Data::String("v1".into())),
            Cell::new((3, 3), Data::String("v2".into())),
        ];
        let range = Range::from_sparse(cells);

        let selection = build_selection(&range, Some("C3:D4"), None, None, 1, MAX_COLUMN_LIMIT).unwrap();
        assert_eq!(selection.start_row, 2);
        assert_eq!(selection.end_row, 3);
        assert_eq!(selection.start_col, 2);
        assert_eq!(selection.end_col, 4);
        assert!(selection.truncated_rows);
        assert!(!selection.truncated_columns);
    }

    #[test]
    fn build_selection_supports_explicit_column_slice_without_range() {
        let cells = vec![
            Cell::new((0, 2), Data::String("h1".into())),
            Cell::new((0, 3), Data::String("h2".into())),
            Cell::new((0, 4), Data::String("h3".into())),
            Cell::new((1, 2), Data::String("v1".into())),
            Cell::new((1, 3), Data::String("v2".into())),
            Cell::new((1, 4), Data::String("v3".into())),
        ];
        let range = Range::from_sparse(cells);

        let selection = build_selection(&range, None, Some(0), Some(3), 2, 1).unwrap();
        assert_eq!(selection.start_col, 3);
        assert_eq!(selection.end_col, 4);
        assert_eq!(selection.requested_start_col, 3);
        assert_eq!(selection.requested_end_col, 4);
        assert!(selection.truncated_columns);
    }
}
