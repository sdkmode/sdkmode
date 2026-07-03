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
    DefaultHinter, EditCommand, Emacs, FileBackedHistory, KeyCode, KeyModifiers, Prompt,
    PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, Reedline, ReedlineEvent,
    Signal, default_emacs_keybindings,
};

use crate::{llm, sandbox, transform};

/// Safety cap on steps within a single turn, so a non-returning agent can't loop
/// forever.
const MAX_STEPS: usize = 12;

/// The framing that opens the rendered program, ahead of the per-SDK blocks.
const SEED_HEADER: &str = r#"// You are a helpful assistant in a REPL that can use authenticated SDKs.
// Think in // comments; hold results in variables (they persist across steps).
// Let code make every decision code can — max, sort, filter, count — and
// console.log only data you must exercise judgement on. Return a value to
// answer the user.
let prompt;
"#;

/// The preamble that opens the rendered program: the header plus every
/// registered SDK's seed block (see [`crate::sdk::Docs::seed`]), in registry
/// order. Valid JavaScript — assembled once, on first use.
static SEED: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let mut seed = String::from(SEED_HEADER);
    for sdk in crate::sdk::registry() {
        seed.push('\n');
        seed.push_str(sdk.docs().seed);
        seed.push('\n');
    }
    seed
});

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
    // Compose multi-line messages: insert a newline instead of submitting.
    // Shift+Enter only reaches us in terminals that report it (Kitty keyboard
    // protocol); Alt+Enter is the universal fallback. Enter still submits.
    let mut keybindings = default_emacs_keybindings();
    let newline = ReedlineEvent::Edit(vec![EditCommand::InsertNewline]);
    keybindings.add_binding(KeyModifiers::SHIFT, KeyCode::Enter, newline.clone());
    keybindings.add_binding(KeyModifiers::ALT, KeyCode::Enter, newline);

    let editor = Reedline::create()
        .with_edit_mode(Box::new(Emacs::new(keybindings)))
        // A pasted block is inserted as one multi-line edit, not submitted
        // line-by-line.
        .use_bracketed_paste(true)
        .with_hinter(Box::new(DefaultHinter::default()));

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
    let mut context = SEED.clone();
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

/// The status line's note for a phase: `thinking · step 2 · $0.011`. No total
/// is shown — [`MAX_STEPS`] is a safety cap, not a plan, so a `/12` would
/// wrongly read as a progress bar. Cost appears once there is any.
fn status_note(phase: &str, step: usize, cost_usd: f64) -> String {
    if cost_usd > 0.0 {
        format!("{phase} · step {step} · ${cost_usd:.3}")
    } else {
        format!("{phase} · step {step}")
    }
}

/// Forwards streamed code to the highlight block, erasing the status line the
/// moment the first delta arrives so the spinner never collides with output,
/// and restoring it while a failed attempt is retried.
struct StatusSink<'a> {
    inner: &'a mut crate::highlight::CodeBlock,
    status: &'a std::cell::RefCell<crate::status::StatusLine>,
}

impl llm::CodeSink for StatusSink<'_> {
    fn on_delta(&mut self, text: &str) {
        self.status.borrow_mut().clear();
        self.inner.on_delta(text);
    }

    fn on_retry(&mut self) {
        self.inner.on_retry();
        self.status.borrow_mut().update("retrying");
    }
}

/// Run one user message as a full agent turn: repeated steps until the agent
/// returns a value, errors out of all its retries, or hits [`MAX_STEPS`].
async fn handle_message(
    message: &str,
    entries: &mut Vec<Entry>,
    session: &mut sandbox::Session,
    llm: &llm::Llm,
) {
    entries.push(Entry::User(message.to_string()));

    let mut steps = 0usize;
    let mut cost_usd = 0.0;
    let started = std::time::Instant::now();

    // The transient spinner filling the two silent phases: waiting on the
    // model, and running a step. In a RefCell because the streaming sink must
    // erase it on the first delta while the `select!` ticker animates it — the
    // two borrows never overlap (single thread; the sink only runs while the
    // completion future is being polled, the ticker only while it is not).
    let status = std::cell::RefCell::new(crate::status::StatusLine::new());
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(120));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    for _ in 0..MAX_STEPS {
        let context = build_context(entries);

        // Stream the model's code live into a highlighted block (on stderr),
        // with a blank line separating steps.
        eprintln!();
        let mut block = crate::highlight::CodeBlock::new(&format!("step {}", steps + 1));
        status
            .borrow_mut()
            .update(&status_note("thinking", steps + 1, cost_usd));
        let completion = {
            let mut sink = StatusSink {
                inner: &mut block,
                status: &status,
            };
            let fut = llm.complete(&context, &mut sink);
            tokio::pin!(fut);
            loop {
                tokio::select! {
                    result = &mut fut => break result,
                    _ = ticker.tick() => status.borrow_mut().tick(),
                }
            }
        };
        status.borrow_mut().clear();

        let code = match completion {
            Ok(completion) if !completion.code.trim().is_empty() => {
                block.finish();
                steps += 1;
                cost_usd += completion.cost_usd;
                completion.code
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

        // Run the step. The spinner advances whenever the guest yields (its
        // awaits: network, timers); a synchronous stretch freezes it, since
        // guest JS shares this thread — that's the known single-thread
        // trade-off, and the watchdog still bounds the step.
        status
            .borrow_mut()
            .update(&status_note("running", steps, cost_usd));
        let eval_result = {
            let fut = session.eval(build_executable(message, &code));
            tokio::pin!(fut);
            loop {
                tokio::select! {
                    result = &mut fut => break result,
                    _ = ticker.tick() => status.borrow_mut().tick(),
                }
            }
        };
        status.borrow_mut().clear();

        let result = match eval_result {
            Ok(result) => result,
            Err(sandbox_error) => {
                let error = format!("sandbox error: {sandbox_error}");
                print_error(&error);
                // Record the failed step so the model's context stays consistent
                // with what actually happened: without this, the next turn would
                // see its last code as if it had never run (empty output, no
                // error), and could not react. Mirror the guest-JS error path
                // (which records `result.error`) with an empty scratchpad.
                entries.push(Entry::Step {
                    code,
                    output: String::new(),
                    error: Some(error),
                });
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
            emit_metrics(steps, cost_usd);
            print_turn_summary(steps, cost_usd, started.elapsed());
            return;
        }

        // No return (and possibly an error to recover from): take another step.
    }

    emit_metrics(steps, cost_usd);
    print_turn_summary(steps, cost_usd, started.elapsed());
    eprintln!("(the assistant did not return an answer after {MAX_STEPS} steps)");
}

/// A dim one-line recap after a turn ends — `(3 steps · $0.012 · 8.4s)` — so
/// cost is visible without setting `SDKMODE_METRICS`. Stays on stderr with the
/// rest of the working noise; plain when stderr is piped.
fn print_turn_summary(steps: usize, cost_usd: f64, elapsed: std::time::Duration) {
    use std::io::IsTerminal;
    if steps == 0 {
        return;
    }
    let plural = if steps == 1 { "" } else { "s" };
    let line = format!(
        "({steps} step{plural} · ${cost_usd:.3} · {:.1}s)",
        elapsed.as_secs_f64()
    );
    if std::io::stderr().is_terminal() {
        eprintln!("\x1b[2m{line}\x1b[0m");
    } else {
        eprintln!("{line}");
    }
}

/// When `SDKMODE_METRICS` is set, print a machine-readable metrics line on
/// stderr at the end of a turn, for harnesses like benchmark.py to parse.
fn emit_metrics(steps: usize, cost_usd: f64) {
    if std::env::var_os("SDKMODE_METRICS").is_some() {
        let metrics = serde_json::json!({ "steps": steps, "cost_usd": cost_usd });
        eprintln!("__sdkmode_metrics {metrics}");
    }
}

/// Run the REPL. Uses the reedline line editor when attached to a terminal, and
/// falls back to reading plain lines from stdin otherwise (so it can be piped).
pub async fn run() -> anyhow::Result<()> {
    use std::io::IsTerminal;

    let mut entries: Vec<Entry> = Vec::new();
    let mut session = sandbox::Session::new().await?;
    let llm = llm::Llm::new();

    if std::io::stdin().is_terminal() {
        run_interactive(&mut entries, &mut session, &llm).await
    } else {
        run_piped(&mut entries, &mut session, &llm).await
    }
}

/// Terminal path: reedline line editing, history, and prompt. Exits on EOF
/// (Ctrl-D); Ctrl-C abandons the current line.
async fn run_interactive(
    entries: &mut Vec<Entry>,
    session: &mut sandbox::Session,
    llm: &llm::Llm,
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
    eprintln!("It may take several steps and answers by returning a value. State persists.");
    eprintln!("Enter sends; Shift+Enter (or Alt+Enter) inserts a newline. Ctrl-D to exit.\n");

    loop {
        match editor.read_line(&prompt) {
            Ok(Signal::Success(line)) => {
                let message = line.trim().to_string();
                if message.is_empty() {
                    continue;
                }
                #[cfg(unix)]
                tokio::select! {
                    _ = handle_message(&message, entries, session, llm) => {}
                    _ = sigint.recv() => {
                        eprintln!("\n(interrupted — Ctrl-D to exit)");
                    }
                }
                #[cfg(not(unix))]
                handle_message(&message, entries, session, llm).await;
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

#[cfg(test)]
mod tests {
    use super::SEED;

    /// The assembled seed must be valid JavaScript — it opens the one growing
    /// program the model completes, so a syntax error here poisons every turn.
    #[test]
    fn assembled_seed_is_valid_javascript() {
        assert!(
            crate::transform::is_parseable(&SEED),
            "assembled seed does not parse:\n{}",
            &*SEED
        );
    }

    /// Every registered SDK's seed block must make it into the assembly, or
    /// the model would never learn that capability exists.
    #[test]
    fn assembled_seed_documents_every_sdk() {
        for sdk in crate::sdk::registry() {
            assert!(
                SEED.contains(sdk.docs().seed),
                "seed is missing the {} block",
                sdk.name()
            );
        }
    }
}

/// Non-terminal path: one message per line on stdin, until EOF. Lines that
/// arrive while a turn is running stay buffered by the OS until we read them.
async fn run_piped(
    entries: &mut Vec<Entry>,
    session: &mut sandbox::Session,
    llm: &llm::Llm,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let message = line.trim().to_string();
        if message.is_empty() {
            continue;
        }
        handle_message(&message, entries, session, llm).await;
    }

    Ok(())
}
