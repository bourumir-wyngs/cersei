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

pub struct SpreadSheetTool;

#[derive(Debug, Clone, Deserialize)]
pub struct SpreadSheetRequest {
    pub action: SpreadSheetAction,
    pub file_path: String,
    pub include_ranges: Option<bool>,
    pub sheet_name: Option<String>,
    pub range: Option<String>,
    pub start_row: Option<usize>,
    pub limit: Option<usize>,
    pub start_col: Option<usize>,
    pub col_limit: Option<usize>,
    pub format: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpreadSheetAction {
    Info,
    Read,
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
impl Tool for SpreadSheetTool {
    fn name(&self) -> &str {
        "SpreadSheet"
    }

    fn description(&self) -> &str {
        "Inspect or read spreadsheet files (Excel, OpenDocument). Use `info` for metadata/sheets, and `read` to extract cell data."
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
                "action": { "type": "string", "enum": ["info", "read"], "description": "Action to perform." },
                "file_path": { "type": "string", "description": "Path to spreadsheet." },
                "include_ranges": { "type": "boolean", "description": "Include used-range in info." },
                "sheet_name": { "type": "string", "description": "Sheet to read (defaults to first)." },
                "range": { "type": "string", "description": "Range to read, e.g. 'A1:D20'." },
                "start_row": { "type": "integer", "description": "Start row for read." },
                "limit": { "type": "integer", "description": "Row limit for read (default 20)." },
                "format": { "type": "string", "enum": ["markdown", "csv"], "description": "Output format." }
            },
            "required": ["action", "file_path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let req: SpreadSheetRequest = match serde_json::from_value(input) {
            Ok(req) => req,
            Err(err) => return ToolResult::error(format!("Invalid input: {}", err)),
        };

        match req.action {
            SpreadSheetAction::Info => execute_info(req, ctx).await,
            SpreadSheetAction::Read => execute_read(req, ctx).await,
        }
    }
}

async fn execute_info(req: SpreadSheetRequest, ctx: &ToolContext) -> ToolResult {
    let path = match resolve_path(ctx, &req.file_path) { Ok(p) => p, Err(e) => return ToolResult::error(e) };
    let workbook = match open_workbook(&path) { Ok(w) => w, Err(e) => return ToolResult::error(e) };
    let sheets = match collect_sheet_summaries(workbook, req.include_ranges.unwrap_or(false)) { Ok(s) => s, Err(e) => return ToolResult::error(e) };

    let mut out = format!("Spreadsheet: {}\nSheets: {}\n\n", path.display(), sheets.len());
    for s in &sheets {
        out.push_str(&format!("- {}: {}x{}, {} cells", s.name, s.total_rows, s.total_columns, s.non_empty_cells));
        if let Some(r) = &s.used_range { out.push_str(&format!(", range {}", r)); }
        out.push('\n');
    }
    ToolResult::success(out).with_metadata(serde_json::json!({ "file_path": path, "sheets": sheets }))
}

async fn execute_read(req: SpreadSheetRequest, ctx: &ToolContext) -> ToolResult {
    let path = match resolve_path(ctx, &req.file_path) { Ok(p) => p, Err(e) => return ToolResult::error(e) };
    let mut workbook = match open_workbook(&path) { Ok(w) => w, Err(e) => return ToolResult::error(e) };
    let sheet_names = workbook.sheet_names().to_vec();
    if sheet_names.is_empty() { return ToolResult::error("No sheets found"); }
    let name = req.sheet_name.clone().unwrap_or_else(|| sheet_names[0].clone());
    let range = match worksheet_range(&mut workbook, &name) { Ok(r) => r, Err(e) => return ToolResult::error(e) };

    let lim = req.limit.unwrap_or(DEFAULT_ROW_LIMIT).min(MAX_ROW_LIMIT);
    let clim = req.col_limit.unwrap_or(MAX_COLUMN_LIMIT).min(MAX_COLUMN_LIMIT);
    let sel = match build_selection(&range, req.range.as_deref(), req.start_row, req.start_col, lim, clim) { Ok(s) => s, Err(e) => return ToolResult::error(e) };

    let sheet_start = range.start().unwrap_or((0, 0));
    let rows: Vec<Vec<String>> = range.rows()
        .skip(sel.start_row.saturating_sub(sheet_start.0 as usize))
        .take(sel.end_row.saturating_sub(sel.start_row))
        .map(|r| r.iter().skip(sel.start_col.saturating_sub(sheet_start.1 as usize)).take(sel.end_col.saturating_sub(sel.start_col)).map(|c| c.to_string()).collect())
        .collect();

    let fmt = req.format.as_deref().unwrap_or("markdown");
    let rendered = if fmt == "csv" { render_csv(&rows) } else { render_markdown_table(&rows) };
    let out = format!("Spreadsheet: {}\nSheet: {}\nRows: {}..{}\n\n{}", path.display(), name, sel.start_row, sel.end_row, rendered);
    ToolResult::success(out).with_metadata(serde_json::json!({ "file_path": path, "sheet": name, "rows": rows.len() }))
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn resolve_path(ctx: &ToolContext, path: &str) -> StringResult<PathBuf> {
    let p = PathBuf::from(path);
    let abs = if p.is_absolute() { p } else { ctx.working_dir.join(p) };
    let can = abs.canonicalize().map_err(|e| format!("Access failed: {}", e))?;
    let root = ctx.working_dir.canonicalize().unwrap_or_else(|_| ctx.working_dir.clone());
    if !can.starts_with(&root) { return Err("Outside workspace".into()); }
    Ok(can)
}

fn open_workbook(path: &Path) -> StringResult<Sheets<std::io::BufReader<fs::File>>> {
    open_workbook_auto(path).map_err(|e| e.to_string())
}

fn worksheet_range(workbook: &mut Sheets<std::io::BufReader<fs::File>>, name: &str) -> StringResult<calamine::Range<calamine::Data>> {
    workbook.worksheet_range(name).map_err(|e| e.to_string())
}

fn collect_sheet_summaries(mut workbook: Sheets<std::io::BufReader<fs::File>>, ranges: bool) -> StringResult<Vec<SheetSummary>> {
    let names = workbook.sheet_names().to_vec();
    let mut sheets = Vec::new();
    for n in names {
        let r = worksheet_range(&mut workbook, &n)?;
        let start = r.start().unwrap_or((0, 0));
        let ur = if ranges && r.width() > 0 && r.height() > 0 {
            Some(format!("{}{}:{}{}", col_label(start.1 as usize), start.0 + 1, col_label(start.1 as usize + r.width() - 1), start.0 as usize + r.height()))
        } else { None };
        let cells = r.rows().map(|row| row.iter().filter(|c| !c.to_string().is_empty()).count()).sum();
        sheets.push(SheetSummary { name: n, total_rows: r.height(), total_columns: r.width(), non_empty_cells: cells, used_range: ur });
    }
    Ok(sheets)
}

fn build_selection(range: &calamine::Range<calamine::Data>, req_range: Option<&str>, s_row: Option<usize>, s_col: Option<usize>, r_lim: usize, c_lim: usize) -> StringResult<Selection> {
    let start = range.start().unwrap_or((0, 0));
    let s_r = start.0 as usize;
    let s_c = start.1 as usize;
    let t_r = range.height();
    let t_c = range.width();

    if let Some(r) = req_range {
        let mut b = parse_a1_range(r)?;
        b.start_row = b.start_row.max(s_r); b.start_col = b.start_col.max(s_c);
        b.end_row = b.end_row.min(s_r + t_r); b.end_col = b.end_col.min(s_c + t_c);
        let u_r = b.end_row; let u_c = b.end_col;
        b.end_row = b.start_row.saturating_add(r_lim).min(b.end_row);
        b.end_col = b.start_col.saturating_add(c_lim).min(b.end_col);
        b.truncated_rows = b.end_row < u_r; b.truncated_columns = b.end_col < u_c;
        Ok(b)
    } else {
        let sr = s_row.unwrap_or(s_r).max(s_r);
        let sc = s_col.unwrap_or(s_c).max(s_c);
        Ok(Selection {
            start_row: sr, end_row: sr.saturating_add(r_lim).min(s_r + t_r),
            start_col: sc, end_col: sc.saturating_add(c_lim).min(s_c + t_c),
            requested_start_row: sr, requested_end_row: sr + r_lim,
            requested_start_col: sc, requested_end_col: sc + c_lim,
            truncated_rows: sr + r_lim < s_r + t_r, truncated_columns: sc + c_lim < s_c + t_c,
        })
    }
}

fn parse_a1_range(text: &str) -> StringResult<Selection> {
    let mut parts = text.trim().split(':');
    let start = parts.next().ok_or("Invalid range")?;
    let end = parts.next().unwrap_or(start);
    let (s_c, s_r) = parse_a1_cell(start)?;
    let (e_c, e_r) = parse_a1_cell(end)?;
    Ok(Selection {
        start_row: s_r, end_row: e_r + 1, start_col: s_c, end_col: e_c + 1,
        requested_start_row: s_r, requested_end_row: e_r + 1,
        requested_start_col: s_c, requested_end_col: e_c + 1,
        truncated_rows: false, truncated_columns: false
    })
}

fn parse_a1_cell(cell: &str) -> StringResult<(usize, usize)> {
    let t = cell.trim();
    let mut ls = String::new(); let mut ds = String::new();
    for c in t.chars() {
        if c == '$' { continue; }
        if c.is_ascii_alphabetic() && ds.is_empty() { ls.push(c.to_ascii_uppercase()); }
        else if c.is_ascii_digit() { ds.push(c); }
        else { return Err(format!("Invalid cell {}", t)); }
    }
    if ls.is_empty() || ds.is_empty() { return Err(format!("Invalid cell {}", t)); }
    let mut col = 0usize;
    for c in ls.chars() { col = col * 26 + (c as u8 - b'A') as usize + 1; }
    Ok((col - 1, ds.parse::<usize>().map_err(|_| "Invalid row")? - 1))
}

fn col_label(mut idx: usize) -> String {
    let mut s = String::new();
    loop { s.insert(0, (b'A' + (idx % 26) as u8) as char); if idx < 26 { break; } idx = (idx / 26) - 1; }
    s
}

fn render_markdown_table(rows: &[Vec<String>]) -> String {
    if rows.is_empty() { return "(empty)".into(); }
    let w = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (i, r) in rows.iter().enumerate() {
        out.push_str("| "); out.push_str(&r.join(" | ")); out.push_str(" |\n");
        if i == 0 { out.push_str("|"); for _ in 0..w { out.push_str(" --- |"); } out.push('\n'); }
    }
    out
}

fn render_csv(rows: &[Vec<String>]) -> String {
    rows.iter().map(|r| r.join(",")).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext {
            session_id: "test".into(),
            working_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            permissions: Arc::new(crate::permissions::AllowAll),
            ..ToolContext::default()
        }
    }

    #[tokio::test]
    async fn test_spreadsheet_info() {
        let tool = SpreadSheetTool;
        let res = tool.execute(json!({ "action": "info", "file_path": "tests/fixtures/Spreadsheet.xlsx" }), &test_ctx()).await;
        assert!(!res.is_error);
        assert!(res.content.contains("Sheets: 2"));
    }
}
