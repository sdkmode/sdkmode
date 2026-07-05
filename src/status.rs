//! A transient, single-line status indicator for the REPL's dead air.
//!
//! Between submitting a message and the first streamed token (a `claude` cold
//! start), and while a step runs in the sandbox, nothing else prints — this
//! fills that silence with a spinner and a short note (`step 2/12 · $0.011`)
//! on stderr. It is strictly transient: rendered with carriage-return +
//! erase-line, never a newline, so clearing it leaves no trace in scrollback
//! and real output always takes its place.
//!
//! When stderr is not a terminal (piped mode, the benchmark harness) it is
//! disabled entirely and emits nothing.

use std::io::{IsTerminal, Write};

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
/// Return to column 0 and erase the line.
const ERASE: &str = "\r\x1b[2K";

pub struct StatusLine {
    enabled: bool,
    /// Whether a status is currently on screen (and must be erased before any
    /// real output prints).
    active: bool,
    frame: usize,
    text: String,
}

impl StatusLine {
    pub fn new() -> Self {
        Self {
            enabled: std::io::stderr().is_terminal(),
            active: false,
            frame: 0,
            text: String::new(),
        }
    }

    /// Show `text` behind the spinner, replacing whatever was showing.
    pub fn update(&mut self, text: &str) {
        self.text = text.to_string();
        self.render();
    }

    /// Advance the spinner one frame. A no-op unless a status is showing, so a
    /// ticker that outlives a [`Self::clear`] cannot redraw a stale line over
    /// real output.
    pub fn tick(&mut self) {
        if !self.active {
            return;
        }
        self.frame = (self.frame + 1) % FRAMES.len();
        self.render();
    }

    /// Erase the status line so real output can take its place. Idempotent.
    pub fn clear(&mut self) {
        if self.enabled && self.active {
            eprint!("{ERASE}");
            let _ = std::io::stderr().flush();
        }
        self.active = false;
    }

    fn render(&mut self) {
        if !self.enabled {
            return;
        }
        eprint!("{ERASE}{DIM}{} {}{RESET}", FRAMES[self.frame], self.text);
        let _ = std::io::stderr().flush();
        self.active = true;
    }
}

impl Default for StatusLine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::StatusLine;

    /// Under the test harness stderr is not a terminal, so the status line is
    /// disabled: the full lifecycle must be silent and panic-free, which is
    /// exactly the piped/benchmark behavior.
    #[test]
    fn disabled_status_line_is_inert() {
        let mut status = StatusLine::new();
        assert!(!status.enabled);
        status.update("thinking");
        status.tick();
        status.clear();
        status.tick(); // ticking after clear must stay a no-op
        assert!(!status.active);
    }
}
