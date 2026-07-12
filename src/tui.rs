//! Full-screen Ratatui interface for the interactive REPL.

use std::io::{self, Stdout};

use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Text},
    widgets::{
        Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Widget, Wrap,
    },
};
use ratatui_textarea::{TextArea, WrapMode};
use tui_syntax_highlight::Highlighter;
use tui_syntax_highlight::syntect::{
    highlighting::ThemeSet, parsing::SyntaxSet, util::LinesWithEndings,
};

pub enum MessageKind {
    User,
    Assistant,
    Code,
    Note,
    Error,
}

pub struct Message {
    pub kind: MessageKind,
    pub text: String,
}

struct HistoryCache {
    width: u16,
    buffer: Buffer,
    height: u16,
}

pub struct App {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    pub input: TextArea<'static>,
    pub messages: Vec<Message>,
    pub status: String,
    scroll: u16,
    follow_tail: bool,
    history_cache: Option<HistoryCache>,
    syntaxes: SyntaxSet,
    highlighter: Highlighter,
}

fn composer_height(lines: &[String], width: u16, terminal_height: u16) -> u16 {
    let inner_width = width.saturating_sub(2).max(1) as usize;
    let rows: usize = lines
        .iter()
        .map(|line| line.chars().count().max(1).div_ceil(inner_width))
        .sum();
    (rows as u16 + 2).clamp(3, terminal_height.saturating_div(2).max(3))
}

/// Ratatui's scrollbar position is an item index (`0..content_length - 1`),
/// while our viewport scroll is a row offset (`0..content_length - viewport`).
/// Scale between them so the first and last viewport positions map exactly to
/// the two ends of the scrollbar track.
fn scrollbar_position(scroll: u16, max_scroll: u16, content_length: u16) -> usize {
    if max_scroll == 0 || content_length <= 1 {
        return 0;
    }
    (scroll as usize * (content_length as usize - 1)) / max_scroll as usize
}

fn has_scroll_range(content_length: u16, viewport_length: u16) -> bool {
    content_length > viewport_length
}

fn new_input() -> TextArea<'static> {
    let mut input = TextArea::default();
    input.set_wrap_mode(WrapMode::WordOrGlyph);
    input.set_cursor_line_style(Style::default());
    input.set_cursor_style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::REVERSED),
    );
    input.set_placeholder_text("Message sdkmode…");
    input.set_placeholder_style(Style::default().fg(Color::DarkGray));
    input.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(" message "),
    );
    input
}

fn insert_composer_newline(input: &mut TextArea<'_>) {
    // A Shift-modified key can leave ratatui-textarea's selection anchor
    // active. `insert_newline` deletes an active selection first, which made
    // Shift+Enter erase the composer. A newline is not a replacement action.
    input.cancel_selection();
    input.insert_newline();
}

impl App {
    pub fn new() -> anyhow::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            )
        )?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;

        let input = new_input();

        let syntaxes = SyntaxSet::load_defaults_newlines();
        let theme = ThemeSet::load_defaults().themes["base16-ocean.dark"].clone();
        let highlighter = Highlighter::new(theme)
            .line_numbers(false)
            .override_background(Color::Reset);

        Ok(Self {
            terminal,
            input,
            messages: Vec::new(),
            status: "Enter send · Shift/Alt+Enter newline · Ctrl-C clear · PgUp/PgDn scroll · Ctrl-D quit"
                .to_string(),
            scroll: 0,
            follow_tail: true,
            history_cache: None,
            syntaxes,
            highlighter,
        })
    }

    pub fn push(&mut self, kind: MessageKind, text: impl Into<String>) {
        self.messages.push(Message {
            kind,
            text: text.into(),
        });
        self.history_cache = None;
        self.follow_tail = true;
        let _ = self.draw();
    }

    pub fn replace_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
        self.history_cache = None;
        self.follow_tail = true;
        let _ = self.draw();
    }

    pub fn append_code(&mut self, text: &str) {
        match self.messages.last_mut() {
            Some(Message {
                kind: MessageKind::Code,
                text: code,
            }) => code.push_str(text),
            _ => self.messages.push(Message {
                kind: MessageKind::Code,
                text: text.to_string(),
            }),
        }
        self.history_cache = None;
        self.follow_tail = true;
        let _ = self.draw();
    }

    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
        let _ = self.draw();
    }

    pub fn take_input(&mut self) -> String {
        let text = self.input.lines().join("\n");
        self.input = new_input();
        text
    }

    pub fn clear_input(&mut self) {
        self.input = new_input();
    }

    pub fn insert_newline(&mut self) {
        insert_composer_newline(&mut self.input);
    }

    pub fn scroll_up(&mut self, rows: u16) {
        self.scroll = self.scroll.saturating_sub(rows);
        self.follow_tail = false;
    }

    pub fn scroll_down(&mut self, rows: u16) {
        self.scroll = self.scroll.saturating_add(rows);
        self.follow_tail = false;
    }

    fn composer_height(&self, width: u16, terminal_height: u16) -> u16 {
        composer_height(self.input.lines(), width, terminal_height)
    }

    fn message_content(&self, message: &Message) -> Text<'static> {
        if matches!(message.kind, MessageKind::Code) {
            let syntax = self
                .syntaxes
                .find_syntax_by_extension("js")
                .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());
            return self
                .highlighter
                .highlight_lines(
                    LinesWithEndings::from(message.text.as_str()),
                    syntax,
                    &self.syntaxes,
                )
                .unwrap_or_else(|_| Text::raw(message.text.clone()));
        }

        let style = match message.kind {
            MessageKind::User => Style::default().fg(Color::Cyan),
            MessageKind::Assistant => Style::default(),
            MessageKind::Note => Style::default().fg(Color::DarkGray),
            MessageKind::Error => Style::default().fg(Color::Red),
            MessageKind::Code => unreachable!(),
        };
        Text::from(
            message
                .text
                .lines()
                .map(|line| Line::styled(line.to_string(), style))
                .collect::<Vec<_>>(),
        )
    }

    fn message_box(&self, message: &Message) -> (Text<'static>, Block<'static>) {
        let (title, color) = match message.kind {
            MessageKind::User => (" you ", Color::Cyan),
            MessageKind::Assistant => (" assistant ", Color::Green),
            MessageKind::Code => (" javascript ", Color::Magenta),
            MessageKind::Note => (" details ", Color::DarkGray),
            MessageKind::Error => (" error ", Color::Red),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(color))
            .title(Line::styled(title, Style::default().fg(color).bold()));
        (self.message_content(message), block)
    }

    /// Render the stacked message cards into an off-screen Ratatui buffer.
    /// The viewport then copies only visible rows, giving us clipped boxes and
    /// styled syntax without hand-drawing either widget.
    fn history_buffer(&self, width: u16) -> (Buffer, u16) {
        let width = width.max(3);
        let inner_width = width.saturating_sub(2).max(1) as usize;
        let cards: Vec<_> = self
            .messages
            .iter()
            .map(|message| {
                let (text, block) = self.message_box(message);
                let content_height: u16 = text
                    .lines
                    .iter()
                    .map(|line| line.width().max(1).div_ceil(inner_width) as u16)
                    .sum::<u16>()
                    .max(1);
                (text, block, content_height.saturating_add(2))
            })
            .collect();
        let height = cards
            .iter()
            .map(|(_, _, height)| height.saturating_add(1))
            .sum::<u16>()
            .saturating_sub(u16::from(!cards.is_empty()))
            .max(1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        let mut y = 0;
        for (text, block, card_height) in cards {
            Paragraph::new(text)
                .block(block)
                .wrap(Wrap { trim: false })
                .render(Rect::new(0, y, width, card_height), &mut buffer);
            y = y.saturating_add(card_height).saturating_add(1);
        }
        (buffer, height)
    }

    pub fn draw(&mut self) -> anyhow::Result<()> {
        let size = self.terminal.size()?;
        let input_height = self.composer_height(size.width, size.height);
        // Reserve the final column for Ratatui's scrollbar.
        let history_width = size.width.saturating_sub(1).max(3);
        if self
            .history_cache
            .as_ref()
            .is_none_or(|cache| cache.width != history_width)
        {
            let (buffer, height) = self.history_buffer(history_width);
            self.history_cache = Some(HistoryCache {
                width: history_width,
                buffer,
                height,
            });
        }
        let history_cache = self
            .history_cache
            .as_ref()
            .expect("history cache was built");
        let history_buffer = &history_cache.buffer;
        let line_count = history_cache.height;
        let status = self.status.clone();
        let follow_tail = self.follow_tail;
        let requested_scroll = self.scroll;
        let input = &self.input;

        let mut actual_scroll = requested_scroll;
        self.terminal.draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(1),
                    Constraint::Length(input_height),
                ])
                .split(frame.area());
            let max_scroll = line_count.saturating_sub(chunks[0].height);
            if follow_tail {
                actual_scroll = max_scroll;
            } else {
                actual_scroll = requested_scroll.min(max_scroll);
            }
            let visible_height = chunks[0]
                .height
                .min(line_count.saturating_sub(actual_scroll));
            for y in 0..visible_height {
                for x in 0..history_width.min(chunks[0].width) {
                    let cell = history_buffer[(x, actual_scroll + y)].clone();
                    frame.buffer_mut()[(chunks[0].x + x, chunks[0].y + y)] = cell;
                }
            }
            if has_scroll_range(line_count, chunks[0].height) {
                let mut scrollbar_state = ScrollbarState::new(line_count as usize)
                    .position(scrollbar_position(actual_scroll, max_scroll, line_count))
                    .viewport_content_length(chunks[0].height as usize);
                frame.render_stateful_widget(
                    Scrollbar::new(ScrollbarOrientation::VerticalRight)
                        .begin_symbol(None)
                        .end_symbol(None)
                        .track_style(Style::default().fg(Color::DarkGray))
                        .thumb_style(Style::default().fg(Color::Cyan)),
                    chunks[0],
                    &mut scrollbar_state,
                );
            }
            frame.render_widget(
                Paragraph::new(status.clone()).style(Style::default().fg(Color::DarkGray)),
                chunks[1],
            );
            frame.render_widget(input, chunks[2]);
        })?;
        self.scroll = actual_scroll;
        Ok(())
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            PopKeyboardEnhancementFlags,
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        composer_height, has_scroll_range, insert_composer_newline, new_input, scrollbar_position,
    };

    #[test]
    fn composer_starts_one_content_row_high() {
        assert_eq!(composer_height(&[String::new()], 80, 24), 3);
    }

    #[test]
    fn fresh_input_is_empty_and_wrapped() {
        let input = new_input();
        assert!(input.is_empty());
        assert_eq!(input.wrap_mode(), ratatui_textarea::WrapMode::WordOrGlyph);
    }

    #[test]
    fn newline_preserves_text_even_when_shift_left_a_selection() {
        let mut input = new_input();
        input.insert_str("first line");
        input.select_all();
        insert_composer_newline(&mut input);
        input.insert_str("second line");
        assert_eq!(input.lines(), ["first line", "second line"]);
    }

    #[test]
    fn composer_grows_for_lines_and_wrapping() {
        assert_eq!(composer_height(&["one".into(), "two".into()], 80, 24), 4);
        assert_eq!(composer_height(&["123456789".into()], 6, 24), 5);
    }

    #[test]
    fn composer_keeps_half_the_terminal_for_messages() {
        assert_eq!(composer_height(&["x".repeat(500)], 20, 20), 10);
    }

    #[test]
    fn scrollbar_reaches_both_ends_of_its_coordinate_system() {
        assert_eq!(scrollbar_position(0, 80, 100), 0);
        assert_eq!(scrollbar_position(80, 80, 100), 99);
        assert_eq!(scrollbar_position(40, 80, 100), 49);
    }

    #[test]
    fn scrollbar_for_content_that_fits_stays_at_the_top() {
        assert_eq!(scrollbar_position(0, 0, 10), 0);
    }

    #[test]
    fn scrollbar_is_hidden_when_everything_fits() {
        assert!(!has_scroll_range(20, 40));
        assert!(!has_scroll_range(40, 40));
        assert!(has_scroll_range(41, 40));
    }
}
