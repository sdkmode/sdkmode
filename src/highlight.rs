//! A streaming syntax highlighter for the code the model writes.
//!
//! Text deltas arrive token-by-token from `claude` (see [`crate::llm`]); we
//! buffer them into lines and, as each line completes, highlight it with syntect
//! and print it inside a bordered block on stderr (the working/code channel).
//! Carrying syntect's per-line parse state gives correct multi-line highlighting
//! while still rendering incrementally.

use std::sync::LazyLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::as_24_bit_terminal_escaped;

use crate::llm::CodeSink;

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

fn javascript_syntax() -> &'static SyntaxReference {
    SYNTAX_SET
        .find_syntax_by_extension("js")
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text())
}

fn theme() -> &'static Theme {
    &THEME_SET.themes["base16-ocean.dark"]
}

/// Streams highlighted code into a bordered block on stderr.
pub struct CodeBlock {
    highlighter: HighlightLines<'static>,
    line: String,
    started: bool,
    color: bool,
}

impl CodeBlock {
    pub fn new() -> Self {
        use std::io::IsTerminal;
        Self {
            highlighter: HighlightLines::new(javascript_syntax(), theme()),
            line: String::new(),
            started: false,
            color: std::io::stderr().is_terminal(),
        }
    }

    /// Flush the buffered line: skip stray markdown fences, print the top border
    /// on first real content, then the highlighted line behind a dim bar. When
    /// stderr is not a terminal, the line is printed plain.
    fn flush_line(&mut self) {
        let trimmed = self.line.trim_end_matches('\n');
        if trimmed.trim_start().starts_with("```") {
            self.line.clear();
            return;
        }

        if !self.color {
            eprint!("{}", self.line);
            self.line.clear();
            return;
        }

        if !self.started {
            eprintln!("{DIM}╭─ code ─────────────────────────────{RESET}");
            self.started = true;
        }

        let ranges = self
            .highlighter
            .highlight_line(&self.line, &SYNTAX_SET)
            .unwrap_or_default();
        let escaped = as_24_bit_terminal_escaped(&ranges, false);
        // The escaped text carries its own trailing newline.
        eprint!("{DIM}│{RESET} {escaped}{RESET}");
        self.line.clear();
    }

    /// Flush any trailing partial line and close the block.
    pub fn finish(&mut self) {
        if !self.line.is_empty() {
            if !self.line.ends_with('\n') {
                self.line.push('\n');
            }
            self.flush_line();
        }
        if self.started {
            eprintln!("{DIM}╰────────────────────────────────────{RESET}");
            self.started = false;
        }
    }
}

impl CodeSink for CodeBlock {
    fn on_delta(&mut self, text: &str) {
        for ch in text.chars() {
            self.line.push(ch);
            if ch == '\n' {
                self.flush_line();
            }
        }
    }

    fn on_retry(&mut self) {
        // Discard whatever streamed from the failed attempt and start clean.
        self.line.clear();
        self.highlighter = HighlightLines::new(javascript_syntax(), theme());
        if self.started {
            eprintln!("{DIM}╰─ (retrying) ───────────────────────{RESET}");
            self.started = false;
        }
    }
}
