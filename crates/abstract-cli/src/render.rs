//! Streaming terminal renderer: markdown, tool badges, thinking, errors.

use crate::theme::Theme;
use crossterm::execute;
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use std::io::{self, Write};
use std::time::Duration;

pub struct StreamRenderer {
    theme: Theme,
    buffer: String,
    in_thinking: bool,
    json_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolConsoleBody {
    text: String,
    kind: ToolConsoleBodyKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolConsoleBodyKind {
    Content,
    Diff,
}

impl StreamRenderer {
    pub fn new(theme: &Theme, json_mode: bool) -> Self {
        Self {
            theme: theme.clone(),
            buffer: String::new(),
            in_thinking: false,
            json_mode,
        }
    }

    /// Push a text delta from the model. Flushes on newlines.
    pub fn push_text(&mut self, delta: &str) {
        if self.json_mode {
            // In JSON mode, print raw events (handled by caller)
            return;
        }

        if self.in_thinking {
            self.end_thinking();
        }

        self.buffer.push_str(delta);

        // Flush on newline boundaries to avoid partial-line flicker
        if let Some(last_nl) = self.buffer.rfind('\n') {
            let to_flush = self.buffer[..=last_nl].to_string();
            self.buffer = self.buffer[last_nl + 1..].to_string();
            self.print_markdown(&to_flush);
        }
    }

    /// Push a thinking delta (dim/italic).
    pub fn push_thinking(&mut self, delta: &str) {
        if self.json_mode {
            return;
        }
        if !self.in_thinking {
            self.in_thinking = true;
            let _ = execute!(
                io::stderr(),
                SetForegroundColor(self.theme.thinking),
                SetAttribute(Attribute::Italic),
                Print("  thinking... "),
            );
        }
        // Don't print thinking content by default — just show spinner
        let _ = delta; // consumed but not displayed
    }

    /// Show a tool start badge.
    pub fn tool_start(&mut self, name: &str, input: &serde_json::Value) {
        if self.json_mode {
            return;
        }
        self.flush();

        let summary = tool_input_summary(name, input);
        let _ = execute!(
            io::stderr(),
            Print("\n"),
            SetForegroundColor(self.theme.tool_badge),
            SetAttribute(Attribute::Bold),
            Print(format!("  [{name}]")),
            ResetColor,
            SetForegroundColor(tool_summary_color(name, &self.theme)),
            Print(format_tool_summary(name, &summary)),
            ResetColor,
            Print("\n"),
        );

        if let Some(body) = tool_input_console_body(name, input) {
            self.print_tool_body(&body);
        }
    }

    /// Show a tool completion.
    pub fn tool_end(&mut self, name: &str, result: &str, is_error: bool, duration: Duration) {
        if self.json_mode {
            return;
        }

        let color = if is_error {
            self.theme.error
        } else {
            self.theme.success
        };
        let icon = if is_error { "x" } else { "+" };
        let ms = duration.as_millis();

        let _ = execute!(
            io::stderr(),
            SetForegroundColor(color),
            Print(format!("  {icon} {name}")),
            ResetColor,
            SetForegroundColor(self.theme.dim),
            Print(format!(" ({ms}ms)")),
            ResetColor,
        );

        // Show truncated result for errors
        if is_error {
            let preview: String = result.chars().take(200).collect();
            let _ = execute!(
                io::stderr(),
                Print("\n"),
                SetForegroundColor(self.theme.error),
                Print(format!("    {preview}")),
                ResetColor,
            );
        }

        if let Some(body) = tool_result_console_body(name, result) {
            self.print_tool_body(&body);
        }

        let _ = execute!(io::stderr(), Print("\n"));
    }

    /// Show a permission prompt and return the rendered description.
    #[allow(dead_code)]
    pub fn permission_header(&self, tool_name: &str, description: &str, level: &str) {
        let _ = execute!(
            io::stderr(),
            Print("\n"),
            SetForegroundColor(self.theme.permission_accent),
            SetAttribute(Attribute::Bold),
            Print(format!("  Permission required: {tool_name}")),
            ResetColor,
            Print("\n"),
            SetForegroundColor(self.theme.dim),
            Print(format!("  {description}")),
            ResetColor,
            Print("\n"),
            SetForegroundColor(self.theme.dim),
            Print(format!("  Risk: {level}")),
            ResetColor,
            Print("\n"),
        );
    }

    /// Show an error message.
    pub fn error(&mut self, msg: &str) {
        if self.json_mode {
            return;
        }
        self.flush();
        let _ = execute!(
            io::stderr(),
            Print("\n"),
            SetForegroundColor(self.theme.error),
            SetAttribute(Attribute::Bold),
            Print("  Error: "),
            ResetColor,
            SetForegroundColor(self.theme.error),
            Print(msg),
            ResetColor,
            Print("\n"),
        );
    }

    /// Flush remaining buffered text.
    pub fn flush(&mut self) {
        if self.json_mode {
            return;
        }
        self.end_thinking();
        if !self.buffer.is_empty() {
            let remaining = std::mem::take(&mut self.buffer);
            self.print_markdown(&remaining);
        }
        let _ = io::stdout().flush();
        let _ = io::stderr().flush();
    }

    /// Notify that the model was switched.
    pub fn model_switched(&mut self, model: &str) {
        if !self.json_mode {
            let _ = execute!(
                io::stderr(),
                Print("\n"),
                SetForegroundColor(self.theme.success),
                Print(format!("  Switched to {model}")),
                ResetColor,
                Print("\n\n"),
            );
        }
    }

    /// Print a completion separator.
    pub fn complete(&mut self) {
        self.flush();
        if !self.json_mode {
            let _ = execute!(io::stdout(), Print("\n"));
        }
    }

    fn end_thinking(&mut self) {
        if self.in_thinking {
            self.in_thinking = false;
            let _ = execute!(
                io::stderr(),
                ResetColor,
                SetAttribute(Attribute::Reset),
                Print("\n"),
            );
        }
    }

    fn print_markdown(&self, text: &str) {
        // Use termimad for rich markdown rendering
        let skin = make_skin(&self.theme);
        let rendered = skin.term_text(text);
        print!("{rendered}");
        let _ = io::stdout().flush();
    }
    fn print_tool_body(&self, body: &ToolConsoleBody) {
        let _ = execute!(io::stderr(), Print("\n"));

        if body.text.is_empty() {
            let _ = execute!(
                io::stderr(),
                SetForegroundColor(self.theme.dim),
                Print("    (empty)\n"),
                ResetColor,
            );
            return;
        }

        for line in body_lines(&body.text) {
            let color = match body.kind {
                ToolConsoleBodyKind::Content => self.theme.text,
                ToolConsoleBodyKind::Diff => match line.chars().next() {
                    Some('+') => self.theme.success,
                    Some('-') => self.theme.error,
                    _ => self.theme.dim,
                },
            };
            let _ = execute!(
                io::stderr(),
                SetForegroundColor(color),
                Print("    "),
                Print(line),
                ResetColor,
                Print("\n"),
            );
        }
    }
}

/// Print a JSON event line (for --json mode).
pub fn print_json_event(event: &cersei_agent::events::AgentEvent) {
    // AgentEvent doesn't derive Serialize, so we manually format key events
    let json = match event {
        cersei_agent::events::AgentEvent::TextDelta(t) => {
            serde_json::json!({"type": "text_delta", "text": t})
        }
        cersei_agent::events::AgentEvent::ThinkingDelta(t) => {
            serde_json::json!({"type": "thinking_delta", "text": t})
        }
        cersei_agent::events::AgentEvent::ToolStart { name, id, input } => {
            serde_json::json!({"type": "tool_start", "name": name, "id": id, "input": input})
        }
        cersei_agent::events::AgentEvent::ToolEnd {
            name,
            id,
            result,
            is_error,
            duration,
        } => {
            serde_json::json!({"type": "tool_end", "name": name, "id": id, "result": result, "is_error": is_error, "duration_ms": duration.as_millis() as u64})
        }
        cersei_agent::events::AgentEvent::CostUpdate {
            turn_cost,
            cumulative_cost,
            input_tokens,
            output_tokens,
        } => {
            serde_json::json!({"type": "cost_update", "turn_cost": turn_cost, "cumulative_cost": cumulative_cost, "input_tokens": input_tokens, "output_tokens": output_tokens})
        }
        cersei_agent::events::AgentEvent::Error(msg) => {
            serde_json::json!({"type": "error", "message": msg})
        }
        cersei_agent::events::AgentEvent::Complete(_) => {
            serde_json::json!({"type": "complete"})
        }
        _ => {
            serde_json::json!({"type": "event"})
        }
    };
    println!("{}", json);
}

fn make_skin(theme: &Theme) -> termimad::MadSkin {
    let mut skin = termimad::MadSkin::default();
    // Customize code block styling
    skin.code_block
        .set_fg(crossterm_to_termimad_color(theme.accent));
    skin.inline_code
        .set_fg(crossterm_to_termimad_color(theme.accent));
    skin.bold.set_fg(crossterm_to_termimad_color(theme.text));
    skin.italic.set_fg(crossterm_to_termimad_color(theme.dim));
    skin
}

fn crossterm_to_termimad_color(c: Color) -> termimad::crossterm::style::Color {
    // termimad re-exports crossterm, so the types are compatible
    match c {
        Color::Black => termimad::crossterm::style::Color::Black,
        Color::DarkGrey => termimad::crossterm::style::Color::DarkGrey,
        Color::Red => termimad::crossterm::style::Color::Red,
        Color::DarkRed => termimad::crossterm::style::Color::DarkRed,
        Color::Green => termimad::crossterm::style::Color::Green,
        Color::DarkGreen => termimad::crossterm::style::Color::DarkGreen,
        Color::Yellow => termimad::crossterm::style::Color::Yellow,
        Color::DarkYellow => termimad::crossterm::style::Color::DarkYellow,
        Color::Blue => termimad::crossterm::style::Color::Blue,
        Color::DarkBlue => termimad::crossterm::style::Color::DarkBlue,
        Color::Magenta => termimad::crossterm::style::Color::Magenta,
        Color::DarkMagenta => termimad::crossterm::style::Color::DarkMagenta,
        Color::Cyan => termimad::crossterm::style::Color::Cyan,
        Color::DarkCyan => termimad::crossterm::style::Color::DarkCyan,
        Color::White => termimad::crossterm::style::Color::White,
        Color::Grey => termimad::crossterm::style::Color::Grey,
        Color::Rgb { r, g, b } => termimad::crossterm::style::Color::Rgb { r, g, b },
        _ => termimad::crossterm::style::Color::Reset,
    }
}

const MAX_REVIEW_PREVIEW_LINES: usize = 5;
const MAX_REVIEW_PREVIEW_CHARS: usize = 512;

fn tool_input_summary(name: &str, input: &serde_json::Value) -> String {
    match name {
        "Bash" | "bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(truncate_review_text)
            .unwrap_or_default(),
        "Process" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(truncate_review_text)
            .unwrap_or_default(),
        "Read" => read_tool_summary(input),
        "Write" => write_tool_summary(input),
        "Edit" | "Sed" | "sed" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Patch" | "patch" => input
            .get("patch")
            .and_then(|v| v.as_str())
            .map(truncate_review_text)
            .unwrap_or_default(),
        "Revert" | "revert" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("last edit")
            .to_string(),
        "Glob" | "glob" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Grep" | "grep" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|s| truncate(s, 60))
            .unwrap_or_default(),
        _ => {
            let s = serde_json::to_string(input).unwrap_or_default();
            truncate(&s, 80)
        }
    }
}

fn tool_summary_color(name: &str, theme: &Theme) -> Color {
    match name {
        "Bash" | "bash" | "Process" => theme.review_text,
        _ => theme.dim,
    }
}

fn format_tool_summary(name: &str, summary: &str) -> String {
    let mut lines = summary.lines();
    let Some(first) = lines.next() else {
        return String::new();
    };

    let continuation_indent = " ".repeat(name.len() + 5);
    let mut formatted = format!(" {first}");
    for line in lines {
        formatted.push('\n');
        formatted.push_str(&continuation_indent);
        formatted.push_str(line);
    }
    formatted
}

fn truncate_review_text(s: &str) -> String {
    let original_line_count = s.lines().count();
    let mut lines: Vec<&str> = s.lines().take(MAX_REVIEW_PREVIEW_LINES + 1).collect();
    let truncated_by_lines = if original_line_count > MAX_REVIEW_PREVIEW_LINES {
        lines.truncate(MAX_REVIEW_PREVIEW_LINES);
        true
    } else {
        false
    };

    let joined = lines.join("\n");
    let original_char_count = s.chars().count();
    let truncated_by_chars = original_char_count > MAX_REVIEW_PREVIEW_CHARS;
    let mut preview = if truncated_by_chars {
        joined
            .chars()
            .take(MAX_REVIEW_PREVIEW_CHARS)
            .collect::<String>()
    } else {
        joined
    };

    if truncated_by_lines || truncated_by_chars {
        preview.push('…');
    }

    preview
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

fn read_tool_summary(input: &serde_json::Value) -> String {
    let file_path = input
        .get("file_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let start_tag = input.get("start_tag").and_then(|v| v.as_str());
    let end_tag = input.get("end_tag").and_then(|v| v.as_str());
    let before = input.get("before").and_then(|v| v.as_u64()).unwrap_or(0);
    let after = input.get("after").and_then(|v| v.as_u64()).unwrap_or(0);
    let length = input.get("length").and_then(|v| v.as_u64());
    let search = input
        .get("search")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let tag_summary = match (start_tag, end_tag) {
        (Some(start_tag), Some(end_tag)) => format!("{start_tag}-{end_tag}"),
        (Some(start_tag), None) => format!("{start_tag}-Tag"),
        (None, _) => "None-Tag".to_string(),
    };

    let mut summary = format!("{file_path} {tag_summary} before={before} after={after}");

    if let Some(search) = search {
        summary.push_str(&format!(" search /{}/", truncate(search, 40)));
    }
    if let Some(length) = length {
        summary.push_str(&format!(" len {length}"));
    }

    summary
}

fn write_tool_summary(input: &serde_json::Value) -> String {
    let file_path = input
        .get("file_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let char_count = input
        .get("content")
        .and_then(|v| v.as_str())
        .map(|content| content.chars().count())
        .unwrap_or(0);

    format!("{file_path} {char_count} chars")
}

fn tool_input_console_body(name: &str, input: &serde_json::Value) -> Option<ToolConsoleBody> {
    if matches!(name, "Write" | "write") {
        return input
            .get("content")
            .and_then(|v| v.as_str())
            .map(|content| ToolConsoleBody {
                text: content.to_string(),
                kind: ToolConsoleBodyKind::Content,
            });
    }

    None
}

fn tool_result_console_body(name: &str, result: &str) -> Option<ToolConsoleBody> {
    if matches!(name, "Patch" | "patch") {
        return extract_result_string_field(result, "patch").map(|patch| ToolConsoleBody {
            text: patch,
            kind: ToolConsoleBodyKind::Diff,
        });
    }

    if matches!(name, "Edit" | "edit") {
        return extract_result_string_field(result, "diff").map(|diff| ToolConsoleBody {
            text: diff,
            kind: ToolConsoleBodyKind::Diff,
        });
    }

    None
}

fn extract_result_string_field(result: &str, field: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(result)
        .ok()
        .and_then(|value| {
            value
                .get(field)
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
}

fn body_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split_inclusive('\n')
        .map(|line| line.strip_suffix('\n').unwrap_or(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_summary_defaults_to_file_path() {
        let summary = tool_input_summary("Read", &serde_json::json!({"file_path": "src/main.rs"}));
        assert_eq!(summary, "src/main.rs None-Tag before=0 after=0");
    }

    #[test]
    fn read_summary_includes_tag_range_and_context() {
        let summary = tool_input_summary(
            "Read",
            &serde_json::json!({
                "file_path": "src/main.rs",
                "start_tag": "tag:10",
                "end_tag": "tag:25",
                "before": 2,
                "after": 3
            }),
        );
        assert_eq!(summary, "src/main.rs tag:10-tag:25 before=2 after=3");
    }

    #[test]
    fn read_summary_includes_length_for_tag_start() {
        let summary = tool_input_summary(
            "Read",
            &serde_json::json!({
                "file_path": "src/main.rs",
                "start_tag": "tag:4",
                "length": 25
            }),
        );
        assert_eq!(summary, "src/main.rs tag:4-Tag before=0 after=0 len 25");
    }

    #[test]
    fn read_summary_includes_search_without_start_tag() {
        let summary = tool_input_summary(
            "Read",
            &serde_json::json!({
                "file_path": "src/main.rs",
                "search": "foo.*bar",
                "before": 1,
                "after": 2,
                "length": 10
            }),
        );
        assert_eq!(
            summary,
            "src/main.rs None-Tag before=1 after=2 search /foo.*bar/ len 10"
        );
    }

    #[test]
    fn read_summary_includes_search_for_tag_start() {
        let summary = tool_input_summary(
            "Read",
            &serde_json::json!({
                "file_path": "src/main.rs",
                "start_tag": "tag:4",
                "search": "todo",
                "length": 25
            }),
        );
        assert_eq!(summary, "src/main.rs tag:4-Tag before=0 after=0 search /todo/ len 25");
    }
    #[test]
    fn write_summary_includes_char_count() {
        let summary = tool_input_summary(
            "Write",
            &serde_json::json!({"file_path": "src/main.rs", "content": "hello"}),
        );
        assert_eq!(summary, "src/main.rs 5 chars");
    }

    #[test]
    fn write_summary_counts_unicode_chars() {
        let summary = tool_input_summary(
            "Write",
            &serde_json::json!({"file_path": "src/main.rs", "content": "aé🙂"}),
        );
        assert_eq!(summary, "src/main.rs 3 chars");
    }

    #[test]
    fn write_console_body_uses_full_content() {
        let body = tool_input_console_body(
            "Write",
            &serde_json::json!({"file_path": "src/main.rs", "content": "alpha\nbeta\n"}),
        )
        .unwrap();

        assert_eq!(
            body,
            ToolConsoleBody {
                text: "alpha\nbeta\n".to_string(),
                kind: ToolConsoleBodyKind::Content,
            }
        );
    }

    #[test]
    fn edit_console_body_extracts_diff_from_result() {
        let body = tool_result_console_body(
            "Edit",
            r#"{"ok":true,"diff":"--- old\n+++ new\n@@ tags @@\n-old\n+new"}"#,
        )
        .unwrap();

        assert_eq!(
            body,
            ToolConsoleBody {
                text: "--- old\n+++ new\n@@ tags @@\n-old\n+new".to_string(),
                kind: ToolConsoleBodyKind::Diff,
            }
        );
    }

    #[test]
    fn patch_console_body_extracts_patch_from_result() {
        let body =
            tool_result_console_body("Patch", r#"{"ok":true,"patch":"@@\n-old\n+new"}"#).unwrap();

        assert_eq!(
            body,
            ToolConsoleBody {
                text: "@@\n-old\n+new".to_string(),
                kind: ToolConsoleBodyKind::Diff,
            }
        );
    }

    #[test]
    fn body_lines_preserve_blank_lines() {
        let lines: Vec<&str> = body_lines("alpha\n\nbeta").collect();
        assert_eq!(lines, vec!["alpha", "", "beta"]);
    }
}
