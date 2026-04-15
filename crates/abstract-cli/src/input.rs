//! Input handler: rustyline wrapper with history and completions.

use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{
    Cmd, ConditionalEventHandler, Config, Editor, Event, EventContext, EventHandler, Helper,
    KeyCode, KeyEvent, Modifiers, RepeatCount,
};
use std::borrow::Cow;

use crate::config;

// ─── Completer helper ──────────────────────────────────────────────────────

#[derive(Clone)]
struct AbstractHelper {
    commands: Vec<String>,
}

impl AbstractHelper {
    fn new() -> Self {
        let commands = vec![
            "/help", "/clear", "/compact", "/cost", "/commit", "/review", "/memory", "/model",
            "/config", "/diff", "/resume", "/exit", "/quit",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        Self { commands }
    }
}

impl Completer for AbstractHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        // Slash command completion
        if line.starts_with('/') {
            let candidates: Vec<Pair> = self
                .commands
                .iter()
                .filter(|c| c.starts_with(line))
                .map(|c| Pair {
                    display: c.clone(),
                    replacement: c.clone(),
                })
                .collect();
            return Ok((0, candidates));
        }

        Ok((pos, vec![]))
    }
}

impl Hinter for AbstractHelper {
    type Hint = String;
    fn hint(&self, _line: &str, _pos: usize, _ctx: &rustyline::Context<'_>) -> Option<String> {
        None
    }
}

impl Highlighter for AbstractHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        Cow::Borrowed(prompt)
    }
}

fn has_unescaped_trailing_backslash(input: &str) -> bool {
    let line = input.strip_suffix('\n').unwrap_or(input);
    let trailing_backslashes = line.chars().rev().take_while(|&c| c == '\\').count();
    trailing_backslashes % 2 == 1
}

impl Validator for AbstractHelper {
    fn validate(&self, ctx: &mut ValidationContext<'_>) -> rustyline::Result<ValidationResult> {
        let input = ctx.input();
        if (!input.is_empty() && input.ends_with('\n')) || has_unescaped_trailing_backslash(input) {
            Ok(ValidationResult::Incomplete)
        } else {
            Ok(ValidationResult::Valid(None))
        }
    }
}
impl Helper for AbstractHelper {}

struct ShiftEnterHandler;

impl ConditionalEventHandler for ShiftEnterHandler {
    fn handle(&self, evt: &Event, _: RepeatCount, _: bool, ctx: &EventContext<'_>) -> Option<Cmd> {
        if matches!(evt, Event::KeySeq(seq) if seq.len() == 1 && seq[0] == KeyEvent(KeyCode::Enter, Modifiers::SHIFT))
        {
            return Some(Cmd::Insert(1, "\n".to_string()));
        }

        if matches!(evt, Event::KeySeq(seq) if seq.len() == 1 && seq[0] == KeyEvent(KeyCode::Enter, Modifiers::NONE))
        {
            let input = ctx.line();
            if !input.is_empty() && input.ends_with('\n') {
                return Some(Cmd::AcceptLine);
            }
        }

        None
    }
}

// ─── Input reader ──────────────────────────────────────────────────────────

pub struct InputReader {
    editor: Editor<AbstractHelper, rustyline::history::DefaultHistory>,
}

impl InputReader {
    pub fn new() -> anyhow::Result<Self> {
        let config = Config::builder()
            .auto_add_history(true)
            .max_history_size(1000)?
            .build();

        let mut editor = Editor::with_config(config)?;
        editor.set_helper(Some(AbstractHelper::new()));
        editor.bind_sequence(
            Event::from(KeyEvent(KeyCode::Enter, Modifiers::SHIFT)),
            EventHandler::Conditional(Box::new(ShiftEnterHandler)),
        );
        editor.bind_sequence(
            Event::from(KeyEvent(KeyCode::Enter, Modifiers::NONE)),
            EventHandler::Conditional(Box::new(ShiftEnterHandler)),
        );

        // Load history
        let history_path = config::history_path();
        if history_path.exists() {
            let _ = editor.load_history(&history_path);
        }

        Ok(Self { editor })
    }

    /// Read a line of input. Returns None on EOF/Ctrl-D.
    pub fn readline(&mut self, prompt: &str) -> Option<String> {
        match self.editor.readline(prompt) {
            Ok(line) => {
                let normalized = line
                    .lines()
                    .map(|line| line.strip_suffix('\\').unwrap_or(line))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim()
                    .to_string();

                if normalized.is_empty() {
                    return Some(String::new());
                }
                Some(normalized)
            }
            Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => None,
            Err(_) => None,
        }
    }

    /// Save history to disk.
    pub fn save_history(&mut self) {
        let history_path = config::history_path();
        if let Some(parent) = history_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = self.editor.save_history(&history_path);
    }
}
