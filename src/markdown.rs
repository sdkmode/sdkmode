//! Renders the agent's answer as markdown for the terminal.
//!
//! The model writes its answer in markdown (bold, headings, lists, inline code),
//! so we render that to ANSI when stdout is a terminal, and fall back to plain
//! text when it is piped so scripts get clean output.

use std::io::IsTerminal;

use termimad::Alignment;
use termimad::MadSkin;
use termimad::crossterm::style::{Attribute, Color};

/// A skin with left-aligned coloured headings (no underline), bright bold, and
/// coloured inline code.
fn skin() -> MadSkin {
    let mut skin = MadSkin::default();
    for header in skin.headers.iter_mut() {
        header.compound_style.set_fg(Color::Cyan);
        header.compound_style.remove_attr(Attribute::Underlined);
        header.compound_style.add_attr(Attribute::Bold);
        header.align = Alignment::Left;
    }
    skin.bold.set_fg(Color::White);
    skin.inline_code.set_fg(Color::Magenta);
    skin
}

/// Print the agent's answer: rendered markdown on a terminal, plain otherwise.
pub fn print_answer(answer: &str) {
    if std::io::stdout().is_terminal() {
        skin().print_text(answer);
    } else {
        println!("{answer}");
    }
}

#[cfg(test)]
mod tests {
    use super::skin;

    #[test]
    fn renders_bold_with_ansi() {
        let rendered = skin().text("**bold** word", Some(80)).to_string();
        assert!(rendered.contains('\u{1b}'), "expected ANSI escapes: {rendered:?}");
        assert!(rendered.contains("bold"), "{rendered:?}");
    }
}
