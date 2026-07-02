//! Drives the `claude` CLI as a raw JavaScript completion engine.
//!
//! There is no MCP here: we hand `claude` a JavaScript module (the REPL history
//! rendered as source) and ask it to write the code that should run next. The
//! REPL then executes that code itself in the sandbox. `claude` is run headless
//! (`-p`), single-turn, with every agent tool disabled, so it behaves as a pure
//! text completion rather than an autonomous agent.

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Receives the model's streamed output so it can be rendered as it arrives.
pub trait CodeSink {
    /// A chunk of generated text just arrived.
    fn on_delta(&mut self, text: &str);
    /// The previous attempt failed; discard anything shown and reset.
    fn on_retry(&mut self);
}

/// Instruction layered onto the model so it answers with bare JavaScript.
const SYSTEM_PROMPT: &str = "\
You are the engine of a stateful, multi-step JavaScript REPL agent.

You receive the session so far as a JavaScript program, ending in a line \
`prompt = \"<the user's latest message>\";`. Reply with ONLY the JavaScript \
source for the NEXT step — no markdown, no code fences, no prose outside // \
comments. Plain English on its own line is a syntax error.

How a turn works:
- Each reply is one step that runs immediately. You then see its output and may \
write another step.
- `console.log(...)` is your scratchpad. Only log when it is either (a) \
intermediate state you need to read/parse yourself before deciding your next \
step, or (b) something you must remember for a later step but will not show the \
user. Otherwise do not log — and never log data you are about to `return` (that \
just wastes tokens).
- You never see a step's output until after the step runs. So when your answer \
depends on data a step produces — fetched page text, a query result, file \
contents — that step must STOP without returning: capture what you got and end \
the step, actually read the output, then answer on the NEXT step. Never write a \
comment guessing what the output will say, and never `return` an answer built \
from output you have not read back — you would be inventing it, and the run will \
show you were wrong.
- For a big or multi-part job, work in steps and carry intermediate results in \
variables, which persist across steps. Build a large artifact — a file's \
contents, a big array — once, assign it to a variable, then reuse that variable \
in later steps: do not re-emit the literal and do not log it. So when something \
big is involved, prefer building it in one step and acting on it (push, write, \
upload) in the next; if that step fails, the variable is still there to retry \
with — never rebuild it from scratch.
- To answer the user, you MUST `return` a value (e.g. `return summary;`). \
Returning ends the turn, shows that value to the user, and hands them control. \
Format the returned string as Markdown.
- A bare expression is NOT an answer: a line like `\"hello\"` or `` `# Hi` `` \
just evaluates and is discarded — the turn stays open and the user is left \
waiting. To say something to the user, `return` it.
- If you do not return, you get another step. If your code throws, you get \
another step to fix it — the error is shown to you. Do not return until you \
actually have the answer; take as many steps as you need.
- Return only what the user asked for; use map/filter/reduce to trim data down \
to exactly that, nothing extraneous.

Environment:
- State persists across steps and turns: variables, functions, and classes you \
declare stay available. Build on them instead of recomputing.
- `octokit` (an authenticated @octokit/rest client) and `prompt` (the user's \
latest message) are always provided. Do not redeclare or re-import them.
- A sandboxed Deno runtime; top-level await is allowed. You may read files in \
the working directory, but cannot access environment variables, spawn \
processes, or reach private network hosts.
- Imports are bare specifiers (never URLs) from a small allowlist: \
`@octokit/rest` (GitHub API), `@std/fs` (Deno file helpers), `isomorphic-git` \
plus `isomorphic-git/http/web` (pure-JS git — a `fs` global wired to the working \
directory is provided, so `git.log`/`git.statusMatrix` work on `dir: \".\"`), and \
`@astral/astral` (browser automation). Do not import anything else.
- `browser` is always provided: an Astral browser that lazily launches a \
headless Chrome on first use. Use it directly, e.g. `const page = await \
browser.newPage(url); await page.evaluate(() => document.title)`.";

/// Extra attempts if `claude` exits non-zero (transient cold-start failures).
const RETRIES: usize = 1;

/// One step's result: the extracted JavaScript and what the `claude` call cost.
pub struct Completion {
    pub code: String,
    pub cost_usd: f64,
}

/// Ask `claude` for the next step's JavaScript, streaming its output to `sink`
/// and returning the extracted source plus the step's cost. Retries once on a
/// transient failure.
pub async fn complete(context: &str, sink: &mut dyn CodeSink) -> anyhow::Result<Completion> {
    let mut last_error = None;
    for attempt in 0..=RETRIES {
        if attempt > 0 {
            sink.on_retry();
        }
        match run_claude(context, sink).await {
            Ok((text, cost_usd)) => {
                return Ok(Completion {
                    code: extract_code(&text),
                    cost_usd,
                });
            }
            Err(error) => {
                last_error = Some(error);
                if attempt < RETRIES {
                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                }
            }
        }
    }
    Err(last_error.unwrap())
}

/// Run `claude` once as a pure code-completion engine: its persona is fully
/// replaced by [`SYSTEM_PROMPT`], dynamic context (git status, env) and project
/// settings (CLAUDE.md, MCP servers) are excluded, and every built-in tool is
/// off — so it can only emit the next step's source. Streams text deltas to
/// `sink` and returns the full concatenated output plus the run's reported cost.
async fn run_claude(context: &str, sink: &mut dyn CodeSink) -> anyhow::Result<(String, f64)> {
    let mut child = Command::new("claude")
        .arg("-p")
        .arg("--output-format")
        .arg("stream-json") // stream tokens as they are generated
        .arg("--verbose") // required with stream-json under -p
        .arg("--include-partial-messages")
        .arg("--max-turns")
        .arg("1")
        .arg("--tools")
        .arg("") // disable all built-in tools
        .arg("--setting-sources")
        .arg("") // ignore CLAUDE.md, project/user settings, MCP servers
        .arg("--exclude-dynamic-system-prompt-sections")
        .arg("--system-prompt")
        .arg(SYSTEM_PROMPT) // replace the persona entirely, not append
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            anyhow::anyhow!("failed to launch `claude` (is it installed and on PATH?): {error}")
        })?;

    // The prompt (our JavaScript program) is fed on stdin so large histories
    // don't run into argv length limits.
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("claude stdin was not captured"))?;
        stdin.write_all(context.as_bytes()).await?;
        stdin.shutdown().await?;
    }

    // Drain stderr on a separate task so a large stderr can't fill the pipe and
    // deadlock us while we're busy reading stdout line-by-line below.
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("claude stderr was not captured"))?;
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let mut reader = BufReader::new(stderr);
        // Best-effort: if reading stderr fails we still want the exit status.
        let _ = reader.read_to_string(&mut buf).await;
        buf
    });

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("claude stdout was not captured"))?;
    let mut lines = BufReader::new(stdout).lines();

    let mut full = String::new();
    let mut cost_usd = 0.0;
    while let Some(line) = lines.next_line().await? {
        if let Some(delta) = parse_text_delta(&line) {
            full.push_str(&delta);
            sink.on_delta(&delta);
        } else if let Some(cost) = parse_result_cost(&line) {
            cost_usd = cost;
        }
    }

    let status = child.wait().await?;
    let stderr = stderr_task.await.unwrap_or_default();
    if !status.success() {
        anyhow::bail!("{}", diagnose_failure(&status.to_string(), &stderr));
    }

    Ok((full, cost_usd))
}

/// Build a human-readable diagnostic for a non-zero `claude` exit from the exit
/// status description and its captured stderr. Pure so it can be unit-tested
/// without spawning a process.
///
/// Beyond echoing the status and stderr, it recognizes the failure mode we most
/// worry about: a future `claude` release dropping or renaming one of the
/// unstable flags we depend on, which shows up as an argument-parsing complaint.
fn diagnose_failure(status_desc: &str, stderr: &str) -> String {
    let stderr = stderr.trim();

    if stderr.is_empty() {
        return format!(
            "`claude` exited with {status_desc} but produced no diagnostics; \
             check that `claude` is installed and on PATH and try `claude --version`."
        );
    }

    let mut message = format!("`claude` exited with {status_desc}: {stderr}");

    if looks_like_flag_incompatibility(stderr) {
        message.push_str(
            "\nhint: this looks like a `claude` CLI incompatibility — sdkmode was built \
             against a specific set of `claude` flags; check your `claude` version \
             (`claude --version`) and update sdkmode or claude.",
        );
    }

    message
}

/// True if `stderr` reads like an argument/option parsing failure, i.e. `claude`
/// no longer recognizes a flag we pass. Case-insensitive substring match on a
/// small set of the phrasings CLI parsers use for this.
fn looks_like_flag_incompatibility(stderr: &str) -> bool {
    const MARKERS: &[&str] = &[
        "unrecognized",
        "unknown option",
        "unknown argument",
        "unexpected argument",
        "invalid value",
        "no such option",
    ];
    let lower = stderr.to_lowercase();
    MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Pull `total_cost_usd` out of a `stream-json` `result` line, if present.
fn parse_result_cost(line: &str) -> Option<f64> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("type")?.as_str()? != "result" {
        return None;
    }
    value.get("total_cost_usd")?.as_f64()
}

/// Pull the text out of a `stream-json` `content_block_delta` line, if that's
/// what this line is.
fn parse_text_delta(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("type")?.as_str()? != "stream_event" {
        return None;
    }
    let event = value.get("event")?;
    if event.get("type")?.as_str()? != "content_block_delta" {
        return None;
    }
    let delta = event.get("delta")?;
    if delta.get("type")?.as_str()? != "text_delta" {
        return None;
    }
    Some(delta.get("text")?.as_str()?.to_string())
}

/// Turn `claude`'s reply into runnable source, repairing the two ways the model
/// leaks non-code: a markdown code fence (even with prose before it), and
/// leading prose it forgot to mark as a `//` comment. In both cases the prose is
/// preserved as `//` comments rather than deleted.
fn extract_code(text: &str) -> String {
    let candidate = fenced(text).unwrap_or_else(|| text.trim().to_string());

    if crate::transform::is_parseable(&candidate) {
        return candidate;
    }

    // Prose that wasn't fenced and isn't commented: comment the leading lines.
    comment_leading_prose(&candidate)
}

/// If the model wrapped its code in a ```…``` fence, return the fence's
/// contents, with everything *before* the opening fence preserved as `//`
/// comments. `None` when there is no fence; a missing closing fence is tolerated.
fn fenced(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let open = lines
        .iter()
        .position(|line| line.trim_start().starts_with("```"))?;
    let after_open = &lines[open + 1..];
    let inner = match after_open
        .iter()
        .position(|line| line.trim_start().starts_with("```"))
    {
        Some(close) => &after_open[..close],
        None => after_open,
    };

    let mut out = String::new();
    for line in &lines[..open] {
        out.push_str(&comment_line(line));
        out.push('\n');
    }
    out.push_str(inner.join("\n").trim());
    Some(out.trim().to_string())
}

/// Find the smallest leading prefix such that the remaining lines parse, then
/// comment that prefix out. Only triggers when there *is* parseable code below,
/// so genuinely broken code still surfaces its error.
fn comment_leading_prose(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let limit = lines.len().min(6);
    for split in 1..limit {
        if !crate::transform::is_parseable(&lines[split..].join("\n")) {
            continue;
        }
        let mut out = String::new();
        for line in &lines[..split] {
            out.push_str(&comment_line(line));
            out.push('\n');
        }
        out.push_str(&lines[split..].join("\n"));
        return out.trim_end().to_string();
    }
    code.trim().to_string()
}

/// Prefix a line with `// `, unless it is blank or already a comment.
fn comment_line(line: &str) -> String {
    if line.trim().is_empty() || line.trim_start().starts_with("//") {
        line.to_string()
    } else {
        format!("// {line}")
    }
}

#[cfg(test)]
mod tests {
    use super::{diagnose_failure, extract_code};

    #[test]
    fn diagnose_flags_cli_incompatibility() {
        let msg = diagnose_failure("exit status: 2", "error: unrecognized argument '--tools'");
        assert!(msg.contains("unrecognized argument '--tools'"), "{msg}");
        assert!(msg.contains("claude` CLI incompatibility"), "{msg}");
        assert!(msg.contains("claude --version"), "{msg}");
    }

    #[test]
    fn diagnose_empty_stderr_suggests_version_check() {
        let msg = diagnose_failure("exit status: 1", "   \n  ");
        assert!(msg.contains("no diagnostics"), "{msg}");
        assert!(msg.contains("claude --version"), "{msg}");
        assert!(msg.contains("PATH"), "{msg}");
        assert!(!msg.contains("CLI incompatibility"), "{msg}");
    }

    #[test]
    fn diagnose_other_error_is_verbatim_without_false_hint() {
        let msg = diagnose_failure(
            "exit status: 1",
            "  Error: overloaded_error: the model is overloaded  ",
        );
        // stderr is included verbatim, trimmed.
        assert!(
            msg.contains("Error: overloaded_error: the model is overloaded"),
            "{msg}"
        );
        // No spurious incompatibility hint for unrelated errors.
        assert!(!msg.contains("CLI incompatibility"), "{msg}");
    }

    #[test]
    fn leaves_clean_code_untouched() {
        assert_eq!(extract_code("const x = 5;"), "const x = 5;");
    }

    #[test]
    fn strips_markdown_fence() {
        assert_eq!(extract_code("```js\nconst x = 5;\n```"), "const x = 5;");
    }

    #[test]
    fn comments_leaked_prose_preamble() {
        // Prose the model forgot to comment is preserved as a comment, not deleted.
        let out = extract_code("Here is the code:\nreturn 1 + 1;");
        assert_eq!(out, "// Here is the code:\nreturn 1 + 1;");
        assert!(crate::transform::is_parseable(&out));
    }

    #[test]
    fn recovers_prose_then_fenced_code() {
        // The real failure: a prose sentence (with `'` and backticks) before a
        // fenced code block. The fence isn't at the start, so the old stripper
        // missed it. Now we pull out the fenced code and keep the pre-fence
        // prose as a comment.
        let input = "The workflow only triggers on `push`. I'll trigger it by creating an empty commit.\n\n```javascript\n// Get the current HEAD commit\nconst owner = \"sdkmode\";\nreturn `done ${owner}`;\n```";
        let out = extract_code(input);
        assert!(crate::transform::is_parseable(&out), "not parseable: {out}");
        assert!(
            out.contains("// The workflow only triggers"),
            "prose not commented: {out}"
        );
        assert!(out.contains("const owner"), "code missing: {out}");
    }

    #[test]
    fn leaves_broken_single_line_to_error() {
        // Genuinely broken code (no parseable remainder) is not silently
        // commented away — it's returned so the runtime surfaces the error.
        assert_eq!(extract_code("const = = oops"), "const = = oops");
    }
}
