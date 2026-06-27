//! Natural-language, multi-step agent REPL over the sandbox.
//!
//! The user types English. That becomes `prompt = "..."` and kicks off an agent
//! turn: the LLM ([`crate::llm`]) writes one step of JavaScript, we run it in the
//! persistent session ([`crate::sandbox::Session`]), and:
//!
//!   - `console.log(...)` is the agent's scratchpad (shown to the user as
//!     working notes, fed back as context),
//!   - a `return` value is the answer to the user and ends the turn,
//!   - no return, or an error, means the agent gets another step.
//!
//! The whole conversation — every step's code, scratchpad, and errors — is the
//! context handed back to the model each step, rendered as one growing program.
//! There is no MCP in this path: the model writes code, we run it.

use std::borrow::Cow;

use reedline::{
    DefaultHinter, FileBackedHistory, Prompt, PromptEditMode, PromptHistorySearch,
    PromptHistorySearchStatus, Reedline, Signal,
};

use crate::{llm, sandbox, transform};

/// Safety cap on steps within a single turn, so a non-returning agent can't loop
/// forever.
const MAX_STEPS: usize = 12;

/// The preamble that opens the rendered program. Valid JavaScript and the framing
/// the model sees: `octokit` is authenticated, `prompt` holds the latest message.
const SEED: &str = r#"import { Octokit } from "@octokit/rest";

// You are a helpful assistant in a REPL that can use authenticated SDKs.
// Think in // comments and console.log(). Return a value to answer the user.
const octokit = new Octokit();
let prompt;
"#;

/// One entry in the session transcript that is rendered back to the model.
enum Entry {
    /// A user message: becomes `prompt = "..."`.
    User(String),
    /// One agent step: the code it ran, its scratchpad output, and any error.
    Step {
        code: String,
        output: String,
        error: Option<String>,
    },
    /// The value the agent returned to the user, ending a turn.
    Answer(String),
}

/// A minimal `sdkmode> ` prompt.
struct ReplPrompt;

impl Prompt for ReplPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("sdkmode")
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _edit_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("> ")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("... ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let prefix = match history_search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!(
            "({}reverse-search: {}) ",
            prefix, history_search.term
        ))
    }
}

/// Build the line editor, wiring up file-backed history when a home directory is
/// available so entries persist across sessions.
fn build_editor() -> Reedline {
    let editor = Reedline::create().with_hinter(Box::new(DefaultHinter::default()));

    let Some(mut path) = dirs_home() else {
        return editor;
    };
    path.push(".sdkmode_history");

    match FileBackedHistory::with_file(1000, path) {
        Ok(history) => editor.with_history(Box::new(history)),
        Err(_) => editor,
    }
}

/// Best-effort home directory lookup without pulling in an extra dependency.
fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

/// A `prompt = "...";` line with the message safely encoded as a JS string.
fn prompt_line(message: &str) -> String {
    let encoded = serde_json::to_string(message).unwrap_or_else(|_| "\"\"".to_string());
    format!("prompt = {encoded};\n")
}

/// Render some text as `// ` comment lines under a label, for the model context.
fn comment_block(label: &str, text: &str) -> String {
    if text.trim().is_empty() {
        return String::new();
    }
    let mut block = format!("// {label}:\n");
    for line in text.lines() {
        block.push_str("// ");
        block.push_str(line);
        block.push('\n');
    }
    block
}

/// The growing JavaScript program shown to the model: seed + every entry so far.
fn build_context(entries: &[Entry]) -> String {
    let mut context = String::from(SEED);
    for entry in entries {
        match entry {
            Entry::User(message) => {
                context.push('\n');
                context.push_str(&prompt_line(message));
            }
            Entry::Step {
                code,
                output,
                error,
            } => {
                context.push_str(code.trim());
                context.push('\n');
                context.push_str(&comment_block("output", output));
                if let Some(error) = error {
                    context.push_str(&comment_block("error", error));
                }
            }
            Entry::Answer(value) => {
                context.push_str(&comment_block("returned to user", value));
            }
        }
    }
    context
}

/// The JavaScript executed for one step: set `prompt` on the shared global scope,
/// then run the model's code wrapped for the persistent session.
fn build_executable(message: &str, code: &str) -> String {
    format!(
        "globalThis.{}{}",
        prompt_line(message),
        transform::wrap_turn(code)
    )
}

/// Print the step's scratchpad (`console.log`) output, dimmed on a terminal to
/// keep it visually subordinate to the answer.
fn print_scratchpad(output: &str) {
    use std::io::IsTerminal;
    if output.trim().is_empty() {
        return;
    }
    if std::io::stderr().is_terminal() {
        for line in output.lines() {
            eprintln!("\x1b[2m{line}\x1b[0m");
        }
    } else {
        eprintln!("{output}");
    }
}

/// Print a step error, in red on a terminal.
fn print_error(error: &str) {
    use std::io::IsTerminal;
    if std::io::stderr().is_terminal() {
        eprintln!("\x1b[31m{error}\x1b[0m");
    } else {
        eprintln!("{error}");
    }
}

/// Run one user message as a full agent turn: repeated steps until the agent
/// returns a value, errors out of all its retries, or hits [`MAX_STEPS`].
async fn handle_message(message: &str, entries: &mut Vec<Entry>, session: &mut sandbox::Session) {
    entries.push(Entry::User(message.to_string()));

    for _ in 0..MAX_STEPS {
        let context = build_context(entries);

        // Stream the model's code live into a highlighted block (on stderr),
        // with a blank line separating steps.
        eprintln!();
        let mut block = crate::highlight::CodeBlock::new();
        let code = match llm::complete(&context, &mut block).await {
            Ok(code) if !code.trim().is_empty() => {
                block.finish();
                code
            }
            Ok(_) => {
                block.finish();
                eprintln!("(the assistant returned no code)");
                return;
            }
            Err(error) => {
                block.finish();
                eprintln!("llm error: {error}");
                return;
            }
        };

        let result = match session.eval(build_executable(message, &code)).await {
            Ok(result) => result,
            Err(sandbox_error) => {
                eprintln!("sandbox error: {sandbox_error}");
                return;
            }
        };

        print_scratchpad(&result.output);
        if let Some(error) = &result.error {
            print_error(error);
        }

        let answer = result.value.clone();
        entries.push(Entry::Step {
            code,
            output: result.output,
            error: result.error,
        });

        // A returned value is the answer to the user -> stdout, rendered as
        // markdown and set off by a blank line. Then the turn ends.
        if let Some(answer) = answer {
            eprintln!();
            crate::markdown::print_answer(&answer);
            entries.push(Entry::Answer(answer));
            return;
        }

        // No return (and possibly an error to recover from): take another step.
    }

    eprintln!("(the assistant did not return an answer after {MAX_STEPS} steps)");
}

/// Run the REPL. Uses the reedline line editor when attached to a terminal, and
/// falls back to reading plain lines from stdin otherwise (so it can be piped).
pub async fn run() -> anyhow::Result<()> {
    use std::io::IsTerminal;

    let mut entries: Vec<Entry> = Vec::new();
    let mut session = sandbox::Session::new().await?;

    if std::io::stdin().is_terminal() {
        run_interactive(&mut entries, &mut session).await
    } else {
        run_piped(&mut entries, &mut session).await
    }
}

/// Terminal path: reedline line editing, history, and prompt. Exits on EOF
/// (Ctrl-D); Ctrl-C abandons the current line.
async fn run_interactive(
    entries: &mut Vec<Entry>,
    session: &mut sandbox::Session,
) -> anyhow::Result<()> {
    let mut editor = build_editor();
    let prompt = ReplPrompt;

    // Catch SIGINT so Ctrl-C during an agent turn aborts the turn and returns
    // to the prompt instead of killing the whole process. The child `claude`
    // process is in the same process group and receives the signal too, so no
    // extra cleanup is needed. Ctrl-D is the intended way to exit.
    #[cfg(unix)]
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    eprintln!(
        "sdkmode — describe what you want in English; the assistant writes and runs JavaScript."
    );
    eprintln!(
        "It may take several steps and answers by returning a value. State persists. Ctrl-D to exit.\n"
    );

    loop {
        match editor.read_line(&prompt) {
            Ok(Signal::Success(line)) => {
                let message = line.trim().to_string();
                if message.is_empty() {
                    continue;
                }
                #[cfg(unix)]
                tokio::select! {
                    _ = handle_message(&message, entries, session) => {}
                    _ = sigint.recv() => {
                        eprintln!("\n(interrupted — Ctrl-D to exit)");
                    }
                }
                #[cfg(not(unix))]
                handle_message(&message, entries, session).await;
            }
            Ok(Signal::CtrlC) => continue, // clears the input line; Ctrl-D exits
            Ok(Signal::CtrlD) => break,
            Ok(_) => continue,
            Err(error) => {
                eprintln!("repl error: {error}");
                break;
            }
        }
    }

    Ok(())
}

/// Non-terminal path: one message per line on stdin, until EOF. Lines that
/// arrive while a turn is running stay buffered by the OS until we read them.
async fn run_piped(entries: &mut Vec<Entry>, session: &mut sandbox::Session) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let message = line.trim().to_string();
        if message.is_empty() {
            continue;
        }
        handle_message(&message, entries, session).await;
    }

    Ok(())
}
