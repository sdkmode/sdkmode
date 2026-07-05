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
//!
//! The conversation is also handed *into* the sandbox as the `context` global
//! (see [`crate::transcript`]): an array of `{id, type, ...}` messages the
//! agent may edit or delete to manage its own context. Edits are reconciled
//! after every step, and a deleted step's bindings are deallocated so the
//! heap never outlives its record. Between turns the transcript and the
//! JSON-able variables are snapshotted per working directory (see
//! [`crate::snapshot`]), so a restart looks like a pause between messages —
//! marked only by a `// it is now ...` note when the gap warrants one.

use std::time::Duration;

use tokio::sync::mpsc;

use crate::transcript::{Entry, EntryKind, Transcript};
use crate::{discord_gateway, llm, sandbox, snapshot, transform};

/// One item in the agent's inbox: something to react to. Both input sources —
/// the terminal and (when enabled) the Discord gateway — feed the same
/// channel, so the agent has one queue regardless of where a message came
/// from. Buffered arrivals are drained into context between steps of a turn.
enum Input {
    /// A line typed at the terminal.
    User(String),
    /// A raw Discord `MESSAGE_CREATE` data object, still to be judged by the
    /// guest's `onDiscordEvent` policy.
    Discord(serde_json::Value),
    /// The terminal reached EOF (Ctrl-D): finish any running turn, then exit.
    Quit,
}

/// Safety cap on steps within a single turn, so a non-returning agent can't loop
/// forever.
const MAX_STEPS: usize = 12;

/// A pause long enough to be worth a `// it is now ...` note in the
/// transcript — whether the process restarted during it or not. Shorter gaps
/// change nothing the model could act on.
const GAP_NOTE_AFTER: Duration = Duration::from_secs(10 * 60);

/// The framing that opens the rendered program, ahead of the per-SDK blocks.
/// `{archive}` is replaced with the real archive path when [`SEED`] is
/// assembled — the guest cannot see `$HOME`, so `~` would be useless to it.
const SEED_HEADER: &str = r#"// You are a helpful assistant in a REPL that can use authenticated SDKs.
// Think in // comments. Variables are your memory: anything you assign
// persists across steps and across restarts (plain data survives a restart;
// live handles do not). To remember something, assign it to a variable; to
// recall it, just use it. Durable facts — about the user, their
// preferences — belong in variables, where they follow you everywhere.
// There is no other memory: never store facts in `context`, and never push
// items into it — console.log is the place for working notes.
// `context` is this very conversation: an array of {id, type, ...} messages
// (type "user" | "step" | "answer" | "note"). It is history, not storage.
// Prune it — but delete an item only when the whole item is stale. To drop
// one fact, line, or bulky output from a step, edit its code or output
// string instead. A variable lives only while some step in context declares
// or assigns it, so check what a step declares before deleting it, and keep
// (or re-add) the assignments a summary relies on. When a prune deallocates
// variables, the step's output will say so.
// The last line of this program is a token gauge. Keep context lean; at 95%
// of the budget, pruning stops being optional — returned answers are
// rejected until context fits again.
// This one session spans all your work. When the working directory changes
// (a note will say so), write what is worth keeping about the old project
// to a file under {archive}/ and prune it from context — then read it back
// if the user returns to that project.
// Let code make every decision code can — max, sort, filter, count — and
// console.log only data you must exercise judgement on. Return a value to
// answer the user.
let prompt;
let context;
"#;

/// The preamble that opens the rendered program: the header (with the real
/// archive path substituted in) plus every registered SDK's seed block (see
/// [`crate::sdk::Docs::seed`]), in registry order. Valid JavaScript —
/// assembled once, on first use.
static SEED: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let archive = snapshot::archive_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "~/.sdkmode/archive".to_string());
    let mut seed = SEED_HEADER.replace("{archive}", &archive);
    for sdk in crate::sdk::registry() {
        seed.push('\n');
        seed.push_str(sdk.docs().seed);
        seed.push('\n');
    }
    seed
});

/// Serial stamped into each step's `context` injection and checked on
/// read-back, so a step that failed before its injection ran (e.g. a syntax
/// error) can never be reconciled against the previous step's stale array —
/// which would silently delete the newest entries.
static CTX_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Read back `context` (with its serial) after a step, as one JSON string.
const READ_CONTEXT: &str = r#"(() => {
    try {
        return JSON.stringify({
            serial: globalThis.__sdkmode_ctx_serial ?? 0,
            context: Array.isArray(globalThis.context) ? globalThis.context : null,
        });
    } catch (_) { return ""; }
})()"#;

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

/// Rough token estimate for model context: ~4 bytes per token. Precision is
/// not the point — the model needs a stable, monotonic gauge to prune
/// against, not an exact count.
fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// The context token budget. Defaults to 50k — well inside every model's
/// window; this is a cost-and-discipline line, not a hard technical limit.
/// `SDKMODE_TOKEN_BUDGET` overrides it.
fn token_budget() -> usize {
    std::env::var("SDKMODE_TOKEN_BUDGET")
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .filter(|budget| *budget > 0)
        .unwrap_or(50_000)
}

/// The trailer appended to every rendered context: the size gauge, plus the
/// mandatory-prune instruction once usage crosses 95% of the budget. The
/// forcing function is deliberately soft — only the model can prune, so the
/// harness cannot refuse to run it without deadlocking; it insists instead,
/// on every step, until the context fits again.
fn context_footer(used: usize, budget: usize) -> String {
    let percent = used * 100 / budget.max(1);
    let mut footer = format!("\n// context: ~{used} of {budget} tokens ({percent}%)\n");
    if used * 20 >= budget * 19 {
        footer.push_str(
            "// CONTEXT NEARLY FULL: shrink it now — delete stale entries and edit\n\
             // bulky outputs down to summaries, keeping the declarations still\n\
             // needed. Answers returned while over budget are rejected.\n",
        );
    }
    footer
}

/// The growing JavaScript program shown to the model: seed + every entry so
/// far, closed by the token gauge (see [`context_footer`]).
fn build_context(entries: &[Entry]) -> String {
    let mut context = SEED.clone();
    for entry in entries {
        match &entry.kind {
            EntryKind::User(message) => {
                context.push('\n');
                context.push_str(&prompt_line(message));
            }
            EntryKind::Step {
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
            EntryKind::Answer(value) => {
                context.push_str(&comment_block("returned to user", value));
            }
            EntryKind::Note(text) => {
                context.push('\n');
                for line in text.lines() {
                    context.push_str("// ");
                    context.push_str(line);
                    context.push('\n');
                }
            }
        }
    }
    let footer = context_footer(estimate_tokens(&context), token_budget());
    context.push_str(&footer);
    context
}

/// The JavaScript executed for one step: stamp the context serial, hand the
/// transcript to the guest as `context`, set `prompt`, then run the model's
/// code wrapped for the persistent session. The transcript is double-encoded
/// (a JSON string literal fed to `JSON.parse`) so its content can never break
/// out of the script text.
fn build_executable(message: &str, code: &str, context_json: &str, serial: u64) -> String {
    let context_literal =
        serde_json::to_string(context_json).unwrap_or_else(|_| "\"[]\"".to_string());
    format!(
        "globalThis.__sdkmode_ctx_serial = {serial};\n\
         globalThis.context = JSON.parse({context_literal});\n\
         globalThis.{}{}",
        prompt_line(message),
        transform::wrap_turn(code)
    )
}

/// The host-side script that deallocates bindings orphaned by a context
/// deletion (see [`Transcript::reconcile`]). The finally-lift creates them
/// with `Object.assign`, so they are ordinary configurable properties.
fn dealloc_script(names: &[String]) -> String {
    let encoded = serde_json::to_string(names).unwrap_or_else(|_| "[]".to_string());
    format!("for (const name of {encoded}) delete globalThis[name];")
}

/// The line appended to a pruning step's output naming the bindings the prune
/// deallocated, so collateral damage is visible while it is still one step
/// from repair.
fn dealloc_note(names: &[String]) -> String {
    format!("pruning deallocated: {}", names.join(", "))
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

/// Print a dim harness aside (restore notices and the like) on stderr.
fn print_aside(text: &str) {
    use std::io::IsTerminal;
    if std::io::stderr().is_terminal() {
        eprintln!("\x1b[2m{text}\x1b[0m");
    } else {
        eprintln!("{text}");
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

/// Add one inbox item to the conversation, returning `true` if it produced a
/// message the agent should act on. A terminal line always does; a Discord
/// event does only if the guest's `onDiscordEvent` policy escalates it (the
/// default ignores bots, so the agent never answers itself). `latest` is set
/// to the message text that becomes the runtime `prompt` global for the next
/// step.
fn ingest(
    input: Input,
    transcript: &mut Transcript,
    session: &mut sandbox::Session,
    latest: &mut String,
) -> bool {
    match input {
        Input::Quit => false,
        Input::User(message) => {
            transcript.push_user(&message);
            *latest = message;
            true
        }
        Input::Discord(event) => {
            let event_json = event.to_string();
            // Ask the guest policy what to do — a plain host eval, no model call.
            let decision = session.read_to_string(format!(
                "(() => {{ try {{ return JSON.stringify(globalThis.onDiscordEvent?.({event_json}) \
                 ?? null); }} catch (error) {{ return JSON.stringify({{ __policyError: \
                 String(error) }}); }} }})()"
            ));
            match serde_json::from_str::<serde_json::Value>(&decision) {
                Ok(serde_json::Value::String(prompt)) => {
                    // Give the turn structured access to the event it reacts to.
                    let _ = session
                        .run_host_script(format!("globalThis.lastDiscordEvent = {event_json};"));
                    transcript.push_user(&prompt);
                    *latest = prompt;
                    true
                }
                Ok(serde_json::Value::Object(map)) if map.contains_key("__policyError") => {
                    eprintln!(
                        "onDiscordEvent threw: {}",
                        map["__policyError"].as_str().unwrap_or("?")
                    );
                    false
                }
                // null or any non-string: ignore.
                _ => false,
            }
        }
    }
}

/// Run one user message as a full agent turn (used by the non-interactive
/// piped path). Interactive turns go through [`run_turn`] directly so they can
/// drain buffered inputs between steps.
pub(crate) async fn handle_message(
    message: &str,
    transcript: &mut Transcript,
    session: &mut sandbox::Session,
    llm: &llm::Llm,
) {
    transcript.push_user(message);
    let mut latest = message.to_string();
    run_turn(&mut latest, transcript, session, llm, None).await;
}

/// The step loop of one turn: repeated steps until the agent returns a value,
/// errors out, or hits [`MAX_STEPS`]. Between steps it drains any buffered
/// inbox items (when `inbox` is `Some`) into context, so messages that arrive
/// mid-turn — a follow-up the user typed, another Discord message — are seen
/// by the next step. `latest` is the current runtime `prompt`. Returns whether
/// a Quit was seen while draining, so the caller can exit after the turn ends.
async fn run_turn(
    latest: &mut String,
    transcript: &mut Transcript,
    session: &mut sandbox::Session,
    llm: &llm::Llm,
    mut inbox: Option<&mut mpsc::UnboundedReceiver<Input>>,
) -> bool {
    let mut should_quit = false;

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
        // Drain anything that arrived since the last step into context, so a
        // follow-up message (typed, or from Discord) is seen by this step
        // rather than waiting for a whole new turn.
        if let Some(inbox) = inbox.as_deref_mut() {
            while let Ok(buffered) = inbox.try_recv() {
                match buffered {
                    Input::Quit => should_quit = true,
                    other => {
                        ingest(other, transcript, session, latest);
                    }
                }
            }
        }

        let context = build_context(&transcript.entries);

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
                return should_quit;
            }
            Err(error) => {
                block.finish();
                eprintln!("llm error: {error}");
                return should_quit;
            }
        };

        // Run the step. The spinner advances whenever the guest yields (its
        // awaits: network, timers); a synchronous stretch freezes it, since
        // guest JS shares this thread — that's the known single-thread
        // trade-off, and the watchdog still bounds the step.
        status
            .borrow_mut()
            .update(&status_note("running", steps, cost_usd));
        let serial = CTX_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let executable =
            build_executable(latest, &code, &transcript.to_json().to_string(), serial);
        let eval_result = {
            let fut = session.eval(executable);
            tokio::pin!(fut);
            loop {
                tokio::select! {
                    result = &mut fut => break result,
                    _ = ticker.tick() => status.borrow_mut().tick(),
                }
            }
        };
        status.borrow_mut().clear();

        // Steps are stored with top-level `const` rewritten to `let`: the
        // runtime already erases the distinction (bindings persist as `var`),
        // and any binding can be deleted along with its step, so a `const` in
        // the history would promise a permanence that does not exist.
        let stored_code = transform::const_to_let(&code);

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
                transcript.push_step(stored_code, String::new(), Some(error));
                return should_quit;
            }
        };

        print_scratchpad(&result.output);
        if let Some(error) = &result.error {
            print_error(error);
        }

        // Make any edits the step performed on `context` real: deletions and
        // changes become the history, and a deleted step's bindings are
        // deallocated. This runs before the step's own entry is pushed, so a
        // step can never delete itself mid-flight.
        let doomed = transcript.reconcile(&session.read_to_string(READ_CONTEXT), serial);
        let mut output = result.output;
        if !doomed.is_empty() {
            let _ = session.run_host_script(dealloc_script(&doomed));
            // Echo the collateral into the step's recorded output, so both
            // the model (next step) and the user (scratchpad) see exactly
            // which bindings the prune took with it — while re-declaring
            // them is still one step away.
            let note = dealloc_note(&doomed);
            print_scratchpad(&note);
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&note);
        }

        let answer = result.value.clone();
        transcript.push_step(stored_code, output, result.error);

        // A returned value is the answer to the user -> stdout, rendered as
        // markdown and set off by a blank line. Then the turn ends — unless
        // the context (including this step) is still over budget: a footer
        // instruction alone is ignorable, so the enforcement is here. The
        // answer is refused and the turn continues; the model can prune and
        // return again in the same breath, and MAX_STEPS bounds refusal loops.
        if let Some(answer) = answer {
            let ctx_tokens = estimate_tokens(&build_context(&transcript.entries));
            if ctx_tokens * 20 >= token_budget() * 19 {
                let rejection = format!(
                    "answer rejected: context is still over budget (~{ctx_tokens} of {} \
                     tokens). Shrink it — delete stale entries, edit bulky outputs down \
                     to summaries — then return again.",
                    token_budget()
                );
                print_error(&rejection);
                transcript.push_note(rejection);
                continue;
            }
            eprintln!();
            crate::markdown::print_answer(&answer);
            transcript.push_answer(answer);
            emit_metrics(steps, cost_usd, ctx_tokens);
            print_turn_summary(steps, cost_usd, started.elapsed(), ctx_tokens);
            return should_quit;
        }

        // No return (and possibly an error to recover from): take another step.
    }

    let ctx_tokens = estimate_tokens(&build_context(&transcript.entries));
    emit_metrics(steps, cost_usd, ctx_tokens);
    print_turn_summary(steps, cost_usd, started.elapsed(), ctx_tokens);
    eprintln!("(the assistant did not return an answer after {MAX_STEPS} steps)");
    should_quit
}

/// A token count as a short human figure: `812`, `4k`, `50k`.
fn fmt_tokens(tokens: usize) -> String {
    if tokens < 1000 {
        tokens.to_string()
    } else {
        format!("{}k", (tokens + 500) / 1000)
    }
}

/// A dim one-line recap after a turn ends — `(3 steps · $0.012 · 8.4s · ctx
/// 4k/50k)` — so cost and context pressure are visible without setting
/// `SDKMODE_METRICS`. Stays on stderr with the rest of the working noise;
/// plain when stderr is piped.
fn print_turn_summary(steps: usize, cost_usd: f64, elapsed: std::time::Duration, ctx_tokens: usize) {
    use std::io::IsTerminal;
    if steps == 0 {
        return;
    }
    let plural = if steps == 1 { "" } else { "s" };
    let line = format!(
        "({steps} step{plural} · ${cost_usd:.3} · {:.1}s · ctx {}/{})",
        elapsed.as_secs_f64(),
        fmt_tokens(ctx_tokens),
        fmt_tokens(token_budget())
    );
    if std::io::stderr().is_terminal() {
        eprintln!("\x1b[2m{line}\x1b[0m");
    } else {
        eprintln!("{line}");
    }
}

/// When `SDKMODE_METRICS` is set, print a machine-readable metrics line on
/// stderr at the end of a turn, for harnesses like benchmark.py to parse.
fn emit_metrics(steps: usize, cost_usd: f64, ctx_tokens: usize) {
    if std::env::var_os("SDKMODE_METRICS").is_some() {
        let metrics = serde_json::json!({
            "steps": steps,
            "cost_usd": cost_usd,
            "context_tokens": ctx_tokens,
        });
        eprintln!("__sdkmode_metrics {metrics}");
    }
}

/// An elapsed duration in the units a reader would pick: seconds, then
/// minutes, then hours, then days.
fn human_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 120 {
        format!("{secs} seconds")
    } else if secs < 120 * 60 {
        format!("{} minutes", secs / 60)
    } else if secs < 48 * 3600 {
        format!("{} hours", secs / 3600)
    } else {
        format!("{} days", secs / 86400)
    }
}

/// The `// it is now ...` note marking a long pause. The sandbox is the
/// clock: `new Date()` there is what the model's own code would see.
fn gap_note(session: &mut sandbox::Session, elapsed: Duration) -> String {
    let now = session.read_to_string("new Date().toString()");
    let now = now.trim();
    if now.is_empty() {
        format!(
            "{} have passed since the previous message",
            human_elapsed(elapsed)
        )
    } else {
        format!(
            "it is now {now} ({} since the previous message)",
            human_elapsed(elapsed)
        )
    }
}

/// Resume the previous session for this working directory, if there is one:
/// the transcript comes back as-is, the JSON-able variables go back onto
/// `globalThis`, and the model gets one honest note when the gap is long or
/// some variables (live handles) did not survive.
fn restore_session(transcript: &mut Transcript, session: &mut sandbox::Session) {
    let Some(path) = snapshot::default_path() else {
        return;
    };
    let Some(saved) = snapshot::load(&path) else {
        return;
    };
    let Some(restored) = Transcript::from_json(&saved.entries) else {
        return;
    };
    if restored.entries.is_empty() {
        return;
    }
    let message_count = restored.entries.len();
    *transcript = restored;

    if saved.vars.as_object().is_some_and(|vars| !vars.is_empty()) {
        let _ = session.run_host_script(snapshot::restore_script(&saved.vars));
    }

    let elapsed = snapshot::now_ms()
        .checked_sub(saved.saved_at_ms)
        .map(Duration::from_millis);
    let mut lines: Vec<String> = Vec::new();
    if let Some(elapsed) = elapsed.filter(|e| *e >= GAP_NOTE_AFTER) {
        lines.push(gap_note(session, elapsed));
    }
    if !saved.lost.is_empty() {
        lines.push(format!(
            "these variables did not survive the restart (not plain data): {}",
            saved.lost.join(", ")
        ));
    }
    let cwd = std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !saved.cwd.is_empty() && !cwd.is_empty() && saved.cwd != cwd {
        let archive = snapshot::archive_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "the archive".to_string());
        lines.push(format!(
            "the working directory changed: {} -> {}. archive what is worth \
             keeping about the old project to a file under {}/, keep durable \
             facts in variables, then delete the old project's entries from \
             context — do not carry them into the new project",
            saved.cwd, cwd, archive
        ));
    }
    if !lines.is_empty() {
        transcript.push_note(lines.join("\n"));
    }

    print_aside(&format!("(resumed {message_count} messages from the previous session)"));
}

/// Snapshot the session after a turn: the transcript plus the current values
/// of every name its steps declare. Best-effort — a failed save costs at most
/// this session's changes, never an error in the user's face.
fn save_session(transcript: &Transcript, session: &mut sandbox::Session) {
    if transcript.entries.is_empty() {
        return;
    }
    let Some(path) = snapshot::default_path() else {
        return;
    };
    let names = transcript.persistable_names();
    let collected = session.read_to_string(snapshot::collect_script(&names));
    let (vars, lost) = snapshot::parse_collected(&collected);
    let saved = snapshot::Snapshot {
        saved_at_ms: snapshot::now_ms(),
        cwd: std::env::current_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default(),
        entries: transcript.to_json(),
        vars,
        lost,
    };
    let _ = snapshot::save(&path, &saved);
}

/// Run the REPL. Interactive on a terminal (the inbox-driven loop, with
/// Discord attached when `SDKMODE_DISCORD_TOKEN` is set); otherwise a plain
/// one-message-per-line piped loop (kept reproducible for tests/benchmarks).
pub async fn run() -> anyhow::Result<()> {
    use std::io::IsTerminal;

    let mut transcript = Transcript::new();
    let mut session = sandbox::Session::new().await?;
    let llm = llm::Llm::new();

    if std::io::stdin().is_terminal() {
        // Sessions persist only on the interactive path: piped runs (tests,
        // benchmark.py) must stay reproducible, never inheriting state.
        restore_session(&mut transcript, &mut session);
        run_interactive(&mut transcript, &mut session, &llm).await
    } else {
        run_piped(&mut transcript, &mut session, &llm).await
    }
}

/// Read terminal lines and push them onto the inbox. Runs on a blocking thread
/// (line input is synchronous) and closes the loop with [`Input::Quit`] on EOF
/// (Ctrl-D). Plain cooked-mode reads, so a turn's output can print concurrently
/// without corrupting the display.
fn spawn_terminal_reader(inbox: mpsc::UnboundedSender<Input>) {
    std::thread::spawn(move || {
        let mut line = String::new();
        loop {
            line.clear();
            match std::io::stdin().read_line(&mut line) {
                // EOF (Ctrl-D).
                Ok(0) => {
                    let _ = inbox.send(Input::Quit);
                    break;
                }
                Ok(_) => {
                    let message = line.trim().to_string();
                    if message.is_empty() {
                        continue;
                    }
                    if inbox.send(Input::User(message)).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = inbox.send(Input::Quit);
                    break;
                }
            }
        }
    });
}

/// Interactive path: one agent draining a shared inbox fed by the terminal and
/// (when a token is set) the Discord gateway. Between-step buffering lets a
/// turn absorb messages that arrive while it runs; snapshots persist state
/// across restarts. Exits on Ctrl-D; Ctrl-C aborts the current turn.
async fn run_interactive(
    transcript: &mut Transcript,
    session: &mut sandbox::Session,
    llm: &llm::Llm,
) -> anyhow::Result<()> {
    let (inbox_tx, mut inbox_rx) = mpsc::unbounded_channel::<Input>();
    spawn_terminal_reader(inbox_tx.clone());

    // Attach Discord when a token is present — not a separate mode. The gateway
    // runs as a background source; its raw message events are forwarded onto
    // the same inbox, where the guest `onDiscordEvent` policy judges them.
    let token = discord_gateway::token();
    // With Discord attached the process is a long-running bot: terminal EOF
    // (Ctrl-D, or a closed stdin when run headless) must not kill it — only a
    // signal does. Without Discord it is a plain REPL that exits on Ctrl-D.
    let discord_enabled = token.is_some();
    if let Some(token) = token {
        session
            .run_host_script(discord_gateway::DEFAULT_POLICY)
            .map_err(|error| anyhow::anyhow!("failed to install the Discord policy: {error}"))?;
        let (events_tx, mut events_rx) = mpsc::unbounded_channel::<serde_json::Value>();
        tokio::spawn(discord_gateway::run_into(token, events_tx));
        let forward = inbox_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = events_rx.recv().await {
                if forward.send(Input::Discord(event)).is_err() {
                    break;
                }
            }
        });
        eprintln!("sdkmode — terminal + Discord. The agent reacts to both. Ctrl-D to exit.\n");
    } else {
        eprintln!(
            "sdkmode — describe what you want in English; the assistant writes and runs \
             JavaScript."
        );
        eprintln!("It may take several steps and answers by returning a value. State persists.");
        eprintln!("Enter sends. Ctrl-D to exit.\n");
    }

    // Catch SIGINT so Ctrl-C during a turn aborts it and returns to the inbox
    // instead of killing the process. Ctrl-D is the way to exit.
    #[cfg(unix)]
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    loop {
        // Idle: wait for an item that actually starts a turn. Ignored Discord
        // events (bot messages, policy-declined) don't wake the agent.
        let mut latest = String::new();
        loop {
            let Some(input) = inbox_rx.recv().await else {
                return Ok(());
            };
            if matches!(input, Input::Quit) {
                if discord_enabled {
                    continue; // terminal closed; keep serving Discord
                }
                return Ok(());
            }
            if ingest(input, transcript, session, &mut latest) {
                break;
            }
        }

        let should_quit;
        #[cfg(unix)]
        {
            should_quit = tokio::select! {
                quit = run_turn(&mut latest, transcript, session, llm, Some(&mut inbox_rx)) => quit,
                _ = sigint.recv() => {
                    eprintln!("\n(interrupted — Ctrl-D to exit)");
                    false
                }
            };
        }
        #[cfg(not(unix))]
        {
            should_quit = run_turn(&mut latest, transcript, session, llm, Some(&mut inbox_rx)).await;
        }

        save_session(transcript, session);
        if should_quit && !discord_enabled {
            return Ok(());
        }
    }
}

/// Non-terminal path: one message per line on stdin, until EOF. Lines that
/// arrive while a turn is running stay buffered by the OS until we read them.
async fn run_piped(
    transcript: &mut Transcript,
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
        handle_message(&message, transcript, session, llm).await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{SEED, dealloc_script, human_elapsed};

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

    #[test]
    fn context_footer_gauges_and_escalates_at_95_percent() {
        let calm = super::context_footer(1_000, 50_000);
        assert!(calm.contains("~1000 of 50000 tokens (2%)"), "{calm}");
        assert!(!calm.contains("NEARLY FULL"), "{calm}");

        let at_threshold = super::context_footer(47_500, 50_000);
        assert!(at_threshold.contains("(95%)"), "{at_threshold}");
        assert!(at_threshold.contains("NEARLY FULL"), "{at_threshold}");

        let below = super::context_footer(47_499, 50_000);
        assert!(!below.contains("NEARLY FULL"), "{below}");
    }

    #[test]
    fn rendered_context_ends_with_the_token_gauge() {
        let mut transcript = crate::transcript::Transcript::new();
        transcript.push_user("hello");
        let rendered = super::build_context(&transcript.entries);
        assert!(rendered.contains("\n// context: ~"), "{rendered}");
    }

    #[test]
    fn tokens_format_as_short_figures() {
        assert_eq!(super::fmt_tokens(812), "812");
        assert_eq!(super::fmt_tokens(4_200), "4k");
        assert_eq!(super::fmt_tokens(50_000), "50k");
    }

    #[test]
    fn human_elapsed_picks_readable_units() {
        use std::time::Duration;
        assert_eq!(human_elapsed(Duration::from_secs(45)), "45 seconds");
        assert_eq!(human_elapsed(Duration::from_secs(40 * 60)), "40 minutes");
        assert_eq!(human_elapsed(Duration::from_secs(4 * 3600)), "4 hours");
        assert_eq!(human_elapsed(Duration::from_secs(3 * 86400)), "3 days");
    }

    /// The full pipeline of one step that edits `context`: injection, the
    /// guest's filter, read-back, reconciliation, and deallocation. Deleting
    /// a step from context must delete the binding it declared.
    #[tokio::test]
    async fn a_step_pruning_context_deallocates_the_deleted_bindings() {
        use crate::transcript::{EntryKind, Transcript};

        let _ = deno_tls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut session = crate::sandbox::Session::new().await.expect("session");
        let mut transcript = Transcript::new();

        // A previous turn declared `x` (executed and recorded).
        transcript.push_user("remember 21");
        session
            .eval(super::build_executable(
                "remember 21",
                "let x = 21;",
                &transcript.to_json().to_string(),
                1,
            ))
            .await
            .expect("declare");
        transcript.push_step("let x = 21;".to_string(), String::new(), None);
        transcript.push_answer("saved");

        // The next step prunes every step from context.
        transcript.push_user("clean up");
        let step = session
            .eval(super::build_executable(
                "clean up",
                "context = context.filter((m) => m.type !== \"step\");",
                &transcript.to_json().to_string(),
                2,
            ))
            .await
            .expect("prune");
        assert!(step.error.is_none(), "prune step errored: {:?}", step.error);

        let doomed = transcript.reconcile(&session.read_to_string(super::READ_CONTEXT), 2);
        assert_eq!(doomed, vec!["x"]);
        assert!(
            !transcript
                .entries
                .iter()
                .any(|e| matches!(e.kind, EntryKind::Step { .. })),
            "the step entry must be gone from the transcript"
        );

        session
            .run_host_script(dealloc_script(&doomed))
            .expect("dealloc");
        let after = session
            .eval(crate::transform::wrap_turn("return typeof x;"))
            .await
            .expect("read back");
        assert_eq!(after.value.as_deref(), Some("undefined"));
    }

    /// Deleting a step's record must delete its binding from the live session
    /// — the heap never outlives the context.
    #[tokio::test]
    async fn deallocation_removes_a_binding_from_the_session() {
        let _ = deno_tls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut session = crate::sandbox::Session::new().await.expect("session");
        session
            .eval(crate::transform::wrap_turn("const x = 21;"))
            .await
            .expect("declare");

        session
            .run_host_script(dealloc_script(&["x".to_string()]))
            .expect("dealloc");

        let after = session
            .eval(crate::transform::wrap_turn("return typeof x;"))
            .await
            .expect("read back");
        assert_eq!(after.value.as_deref(), Some("undefined"));
    }
}
