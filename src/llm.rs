//! Drives the `claude` CLI as a raw JavaScript completion engine.
//!
//! There is no MCP here: we hand `claude` a JavaScript module (the REPL history
//! rendered as source) and ask it to write the code that should run next. The
//! REPL then executes that code itself in the sandbox. `claude` is run headless
//! (`-p`), single-turn, with every agent tool disabled, so it behaves as a pure
//! text completion rather than an autonomous agent.

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
- `console.log(...)` is your scratchpad: use it to inspect values and reason \
across steps. It is shown to the user as working notes, not as your answer.
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
- `@octokit/rest` is the only package you may import. Do not import anything else.";

/// Extra attempts if `claude` exits non-zero (transient cold-start failures).
const RETRIES: usize = 1;

/// Ask `claude` for the next step's JavaScript, streaming its output to `sink`
/// and returning the extracted source. Retries once on a transient failure.
pub async fn complete(context: &str, sink: &mut dyn CodeSink) -> anyhow::Result<String> {
    let mut last_error = None;
    for attempt in 0..=RETRIES {
        if attempt > 0 {
            sink.on_retry();
        }
        match run_claude(context, sink).await {
            Ok(text) => return Ok(extract_code(&text)),
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
/// `sink` and returns the full concatenated output.
async fn run_claude(context: &str, sink: &mut dyn CodeSink) -> anyhow::Result<String> {
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
        .stderr(Stdio::inherit())
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

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("claude stdout was not captured"))?;
    let mut lines = BufReader::new(stdout).lines();

    let mut full = String::new();
    while let Some(line) = lines.next_line().await? {
        if let Some(delta) = parse_text_delta(&line) {
            full.push_str(&delta);
            sink.on_delta(&delta);
        }
    }

    let status = child.wait().await?;
    if !status.success() {
        anyhow::bail!("`claude` exited with {status}");
    }

    Ok(full)
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

/// Turn `claude`'s reply into runnable source: strip any markdown fence, and if
/// the result still doesn't parse, drop leading lines (a leaked prose preamble)
/// until it does.
fn extract_code(text: &str) -> String {
    let stripped = strip_code_fences(text);
    if crate::transform::is_parseable(&stripped) {
        return stripped;
    }

    let lines: Vec<&str> = stripped.lines().collect();
    let limit = lines.len().min(6);
    for start in 1..limit {
        let candidate = lines[start..].join("\n");
        if crate::transform::is_parseable(&candidate) {
            return candidate.trim().to_string();
        }
    }

    stripped
}

/// Strip a single surrounding markdown code fence (```js … ```), if the model
/// added one despite instructions. Leaves un-fenced text untouched.
fn strip_code_fences(text: &str) -> String {
    let trimmed = text.trim();
    let Some(after_open) = trimmed.strip_prefix("```") else {
        return trimmed.to_string();
    };

    // Drop the rest of the opening fence line (e.g. "js" in "```js").
    let body = after_open
        .split_once('\n')
        .map(|(_, rest)| rest)
        .unwrap_or("");
    let body = body.trim_end();
    let body = body.strip_suffix("```").unwrap_or(body);
    body.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::extract_code;

    #[test]
    fn leaves_clean_code_untouched() {
        assert_eq!(extract_code("const x = 5;"), "const x = 5;");
    }

    #[test]
    fn strips_markdown_fence() {
        assert_eq!(extract_code("```js\nconst x = 5;\n```"), "const x = 5;");
    }

    #[test]
    fn drops_leaked_prose_preamble() {
        assert_eq!(
            extract_code("Here is the code:\nreturn 1 + 1;"),
            "return 1 + 1;"
        );
    }
}
