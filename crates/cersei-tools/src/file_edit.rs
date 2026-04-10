//! File edit tool: performs exact string replacements or line-range replacements.

use super::*;
use crate::file_history::FileHistory;
use serde::Deserialize;

pub struct FileEditTool;

#[async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &str { "Edit" }

    fn description(&self) -> &str {
        "Edit a file by replacing text. Two modes:\n\
         \n\
         **Mode 1 — String replacement** (provide `old_string` + `new_string`):\n\
         Replace an exact substring. The `old_string` must match the file content \
         exactly, including whitespace and indentation. Always Read the file first \
         to get the exact text. If the match is not unique, provide more surrounding \
         context or set `replace_all`.\n\
         \n\
         **Mode 2 — Line-range replacement** (provide `start_line` + `new_string`):\n\
         Replace lines by number. Use `start_line` alone to replace a single line, \
         or `start_line` + `end_line` for a range (inclusive). Line numbers correspond \
         to the output of the Read tool. To insert before a line without removing it, \
         set `insert_only` to true.\n\
         \n\
         To delete text, set `new_string` to an empty string."
    }

    fn permission_level(&self) -> PermissionLevel { PermissionLevel::Write }
    fn category(&self) -> ToolCategory { ToolCategory::FileSystem }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to edit"
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact text to find and replace (must match file content exactly, including whitespace)"
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement text (use empty string to delete)"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace all occurrences of old_string (default: false)",
                    "default": false
                },
                "start_line": {
                    "type": "integer",
                    "description": "First line number to replace (1-based, as shown by Read). Use instead of old_string for line-based editing."
                },
                "end_line": {
                    "type": "integer",
                    "description": "Last line number to replace, inclusive (defaults to start_line)"
                },
                "insert_only": {
                    "type": "boolean",
                    "description": "If true with start_line, insert new_string before that line instead of replacing it (default: false)",
                    "default": false
                }
            },
            "required": ["file_path", "new_string"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct Input {
            file_path: String,
            old_string: Option<String>,
            new_string: String,
            #[serde(default)]
            replace_all: bool,
            start_line: Option<usize>,
            end_line: Option<usize>,
            #[serde(default)]
            insert_only: bool,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let path = std::path::Path::new(&input.file_path);
        let content = match tokio::fs::read_to_string(path).await {
            Ok(c) => c,
            Err(e) => return ToolResult::error(format!("Failed to read file: {}", e)),
        };

        // Snapshot before mutation
        if let Some(history) = ctx.extensions.get::<FileHistory>() {
            history.snapshot_before_write(&path.to_path_buf(), &content, "edit");
        }

        // Dispatch to the appropriate mode
        if let Some(start_line) = input.start_line {
            edit_by_lines(&input.file_path, &content, start_line, input.end_line, &input.new_string, input.insert_only).await
        } else if let Some(old_string) = &input.old_string {
            edit_by_string(&input.file_path, &content, old_string, &input.new_string, input.replace_all).await
        } else {
            ToolResult::error(
                "Either `old_string` or `start_line` must be provided.\n\
                 - Use `old_string` to find and replace an exact substring.\n\
                 - Use `start_line` to replace lines by number."
            )
        }
    }
}

/// Line-range based editing.
async fn edit_by_lines(
    file_path: &str,
    content: &str,
    start_line: usize,
    end_line: Option<usize>,
    new_string: &str,
    insert_only: bool,
) -> ToolResult {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    if start_line == 0 || start_line > total + 1 {
        return ToolResult::error(format!(
            "start_line {} is out of range (file has {} lines)",
            start_line, total
        ));
    }

    let end_line = end_line.unwrap_or(start_line);
    if end_line < start_line {
        return ToolResult::error(format!(
            "end_line ({}) must be >= start_line ({})",
            end_line, start_line
        ));
    }
    if end_line > total {
        return ToolResult::error(format!(
            "end_line {} is out of range (file has {} lines)",
            end_line, total
        ));
    }

    let idx_start = start_line - 1; // 0-based
    let idx_end = if insert_only { idx_start } else { end_line }; // exclusive end

    let mut result_lines: Vec<&str> = Vec::with_capacity(total);
    result_lines.extend_from_slice(&lines[..idx_start]);

    // Insert the new content (may be multiple lines)
    let new_lines: Vec<&str> = if new_string.is_empty() {
        vec![]
    } else {
        new_string.lines().collect()
    };
    for nl in &new_lines {
        result_lines.push(nl);
    }

    if idx_end < total {
        result_lines.extend_from_slice(&lines[idx_end..]);
    }

    let mut new_content = result_lines.join("\n");
    // Preserve trailing newline if original had one
    if content.ends_with('\n') {
        new_content.push('\n');
    }

    match tokio::fs::write(std::path::Path::new(file_path), &new_content).await {
        Ok(()) => {
            let verb = if insert_only { "Inserted before" } else { "Replaced" };
            let range = if start_line == end_line && !insert_only {
                format!("line {}", start_line)
            } else if insert_only {
                format!("line {}", start_line)
            } else {
                format!("lines {}-{}", start_line, end_line)
            };
            ToolResult::success(format!(
                "{} {} in {} ({} lines written)",
                verb, range, file_path, new_lines.len()
            ))
        }
        Err(e) => ToolResult::error(format!("Failed to write file: {}", e)),
    }
}

/// Exact-string based editing with diagnostic error messages.
async fn edit_by_string(
    file_path: &str,
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> ToolResult {
    if old_string.is_empty() {
        return ToolResult::error(
            "old_string is empty. To insert text, use start_line with insert_only, \
             or use the Write tool to write the whole file."
        );
    }

    if content.contains(old_string) {
        // Exact case-sensitive match
        let new_content = if replace_all {
            content.replace(old_string, new_string)
        } else {
            let count = content.matches(old_string).count();
            if count > 1 {
                return ToolResult::error(format!(
                    "old_string matches {} locations. Provide more surrounding context to make it unique, \
                     or set replace_all to true.",
                    count
                ));
            }
            content.replacen(old_string, new_string, 1)
        };
        write_content(file_path, &new_content).await
    } else {
        // No exact match — try case-insensitive fallback
        match try_case_insensitive_replace(content, old_string, new_string, replace_all) {
            CaseInsensitiveResult::SingleMatch(new_content) => {
                return write_content(file_path, &new_content).await;
            }
            CaseInsensitiveResult::MultipleMatches(count) => {
                return ToolResult::error(format!(
                    "old_string not found with exact case in {}. \
                     Found {} case-insensitive matches — check the case of your query.",
                    file_path, count
                ));
            }
            CaseInsensitiveResult::NoMatch => {}
        }

        // No match — produce a diagnostic error
        ToolResult::error(build_not_found_diagnostic(file_path, content, old_string))
    }
}

async fn write_content(file_path: &str, content: &str) -> ToolResult {
    match tokio::fs::write(std::path::Path::new(file_path), content).await {
        Ok(()) => ToolResult::success(format!(
            "The file {} has been updated successfully.",
            file_path
        )),
        Err(e) => ToolResult::error(format!("Failed to write file: {}", e)),
    }
}

enum CaseInsensitiveResult {
    SingleMatch(String),
    MultipleMatches(usize),
    NoMatch,
}

/// Try case-insensitive matching.
fn try_case_insensitive_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> CaseInsensitiveResult {
    let content_lower = content.to_lowercase();
    let old_lower = old_string.to_lowercase();

    let matches: Vec<usize> = content_lower
        .match_indices(&old_lower)
        .map(|(pos, _)| pos)
        .collect();

    if matches.is_empty() {
        return CaseInsensitiveResult::NoMatch;
    }

    if !replace_all && matches.len() > 1 {
        return CaseInsensitiveResult::MultipleMatches(matches.len());
    }

    // Build the result by replacing matched ranges (iterate in reverse to preserve offsets)
    let mut result = content.to_string();
    for &pos in matches.iter().rev() {
        result.replace_range(pos..pos + old_string.len(), new_string);
        if !replace_all {
            break;
        }
    }
    CaseInsensitiveResult::SingleMatch(result)
}

/// Build a helpful error message when old_string is not found.
fn build_not_found_diagnostic(file_path: &str, content: &str, old_string: &str) -> String {
    let mut msg = format!("old_string not found in {}\n", file_path);

    // Check for whitespace-only differences
    let normalized_content = normalize_whitespace(content);
    let normalized_old = normalize_whitespace(old_string);
    if normalized_content.contains(&normalized_old) {
        msg.push_str(
            "\nHint: A match exists if whitespace differences are ignored. \
             The old_string likely has incorrect indentation or trailing spaces. \
             Re-read the file and copy the exact text including whitespace."
        );
        // Find approximately where the match is
        if let Some(line_num) = find_approx_line(&normalized_content, &normalized_old, content) {
            msg.push_str(&format!(" The closest match is near line {}.", line_num));
        }
        return msg;
    }

    // Check if all individual lines exist (possibly non-contiguous or reordered)
    let old_lines: Vec<&str> = old_string.lines().collect();
    if old_lines.len() > 1 {
        let content_lines: Vec<&str> = content.lines().collect();
        let missing: Vec<&&str> = old_lines.iter()
            .filter(|l| !l.trim().is_empty())
            .filter(|l| !content_lines.iter().any(|cl| cl == *l))
            .collect();
        if missing.is_empty() {
            msg.push_str(
                "\nHint: All lines from old_string exist in the file but not as a contiguous block. \
                 The lines may be in a different order or have other lines between them. \
                 Re-read the file to get the exact contiguous text."
            );
            return msg;
        }
        if missing.len() < old_lines.len() {
            msg.push_str(&format!(
                "\nHint: Some lines from old_string were not found in the file:\n"
            ));
            for (i, line) in missing.iter().enumerate() {
                if i >= 3 {
                    msg.push_str(&format!("  ... and {} more\n", missing.len() - 3));
                    break;
                }
                msg.push_str(&format!("  {:?}\n", line));
            }
            msg.push_str("Re-read the file to get the current content.");
            return msg;
        }
    }

    // Find the best partial match to give a location hint
    if let Some((line_num, similarity)) = find_best_line_match(content, old_string) {
        msg.push_str(&format!(
            "\nHint: The closest match is near line {} ({:.0}% similar). \
             Re-read the file around that area to get the exact text.",
            line_num, similarity * 100.0
        ));
    } else {
        msg.push_str(
            "\nHint: No similar text found. The file content may have changed. \
             Re-read the file before editing."
        );
    }

    msg
}

/// Collapse runs of whitespace to single spaces for comparison.
fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Given a normalized match, find the approximate line number in the original content.
fn find_approx_line(normalized_content: &str, normalized_old: &str, original: &str) -> Option<usize> {
    let match_pos = normalized_content.find(normalized_old)?;
    // Count words before the match position, then map back to original line
    let target_count = normalized_content[..match_pos].split_whitespace().count();
    let mut word_count = 0;
    for (i, line) in original.lines().enumerate() {
        word_count += line.split_whitespace().count();
        if word_count >= target_count {
            return Some(i + 1);
        }
    }
    None
}

/// Find the best matching region in the file for a multi-line old_string.
/// Returns (line_number, similarity_score).
fn find_best_line_match(content: &str, old_string: &str) -> Option<(usize, f64)> {
    let content_lines: Vec<&str> = content.lines().collect();
    let old_lines: Vec<&str> = old_string.lines().collect();

    if old_lines.is_empty() || content_lines.is_empty() {
        return None;
    }

    // For single-line old_string, compare against each line
    if old_lines.len() == 1 {
        let target = old_lines[0].trim();
        if target.is_empty() {
            return None;
        }
        let mut best_score = 0.0_f64;
        let mut best_line = 0;
        for (i, line) in content_lines.iter().enumerate() {
            let score = line_similarity(target, line.trim());
            if score > best_score {
                best_score = score;
                best_line = i + 1;
            }
        }
        if best_score > 0.4 {
            return Some((best_line, best_score));
        }
        return None;
    }

    // For multi-line: slide a window of the same size and find best match
    let window_size = old_lines.len();
    if window_size > content_lines.len() {
        return None;
    }

    let mut best_score = 0.0_f64;
    let mut best_line = 0;
    for start in 0..=(content_lines.len() - window_size) {
        let mut total = 0.0;
        for (j, old_line) in old_lines.iter().enumerate() {
            total += line_similarity(old_line.trim(), content_lines[start + j].trim());
        }
        let avg = total / window_size as f64;
        if avg > best_score {
            best_score = avg;
            best_line = start + 1;
        }
    }

    if best_score > 0.4 {
        Some((best_line, best_score))
    } else {
        None
    }
}

/// Simple character-level similarity between two strings (Sørensen–Dice on bigrams).
fn line_similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a.len() < 2 || b.len() < 2 {
        return if a == b { 1.0 } else { 0.0 };
    }

    let bigrams_a: Vec<(char, char)> = a.chars().zip(a.chars().skip(1)).collect();
    let bigrams_b: Vec<(char, char)> = b.chars().zip(b.chars().skip(1)).collect();

    let mut matches = 0;
    let mut used = vec![false; bigrams_b.len()];
    for ba in &bigrams_a {
        for (j, bb) in bigrams_b.iter().enumerate() {
            if !used[j] && ba == bb {
                matches += 1;
                used[j] = true;
                break;
            }
        }
    }

    (2.0 * matches as f64) / (bigrams_a.len() + bigrams_b.len()) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_whitespace() {
        assert_eq!(normalize_whitespace("  foo   bar  "), "foo bar");
        assert_eq!(normalize_whitespace("a\n  b\n  c"), "a b c");
    }

    #[test]
    fn test_line_similarity() {
        assert!((line_similarity("hello world", "hello world") - 1.0).abs() < f64::EPSILON);
        assert!(line_similarity("hello world", "hello worl") > 0.8);
        assert!(line_similarity("abcdef", "xyz") < 0.3);
    }

    #[test]
    fn test_find_best_line_match_single() {
        let content = "fn main() {\n    println!(\"hello\");\n}\n";
        let old = "    println!(\"helo\");";
        let result = find_best_line_match(content, old);
        assert!(result.is_some());
        let (line, score) = result.unwrap();
        assert_eq!(line, 2);
        assert!(score > 0.7);
    }

    #[test]
    fn test_find_best_line_match_multi() {
        let content = "line 1\nfn foo() {\n    bar();\n}\nline 5\n";
        let old = "fn foo() {\n    baz();\n}";
        let result = find_best_line_match(content, old);
        assert!(result.is_some());
        let (line, _) = result.unwrap();
        assert_eq!(line, 2);
    }

    #[test]
    fn test_diagnostic_whitespace_hint() {
        let content = "    fn foo() {\n        bar();\n    }\n";
        let old_string = "fn foo() {\n    bar();\n}";
        let diag = build_not_found_diagnostic("test.rs", content, old_string);
        assert!(diag.contains("whitespace"));
    }

    #[test]
    fn test_diagnostic_no_match() {
        let content = "fn main() {}\n";
        let old_string = "completely unrelated text that does not exist";
        let diag = build_not_found_diagnostic("test.rs", content, old_string);
        assert!(diag.contains("not found"));
    }

    #[test]
    fn test_case_insensitive_single_match() {
        let content = "Hello World\nfoo bar\n";
        let result = try_case_insensitive_replace(content, "hello world", "hi", false);
        assert!(matches!(result, CaseInsensitiveResult::SingleMatch(ref s) if s == "hi\nfoo bar\n"));
    }

    #[test]
    fn test_case_insensitive_no_match() {
        let content = "Hello World\n";
        let result = try_case_insensitive_replace(content, "goodbye", "hi", false);
        assert!(matches!(result, CaseInsensitiveResult::NoMatch));
    }

    #[test]
    fn test_case_insensitive_ambiguous_without_replace_all() {
        let content = "Hello hello HELLO\n";
        let result = try_case_insensitive_replace(content, "hello", "hi", false);
        assert!(matches!(result, CaseInsensitiveResult::MultipleMatches(3)));
    }

    #[test]
    fn test_case_insensitive_replace_all() {
        let content = "Hello hello HELLO\n";
        let result = try_case_insensitive_replace(content, "hello", "hi", true);
        assert!(matches!(result, CaseInsensitiveResult::SingleMatch(ref s) if s == "hi hi hi\n"));
    }
}
