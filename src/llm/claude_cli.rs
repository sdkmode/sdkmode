//! [`LlmProvider`] that drives the `claude` CLI as a raw completion engine.
//!
//! There is no MCP here: we hand `claude` a JavaScript module (the REPL history
//! rendered as source) and ask it to write the code that should run next. The
//! REPL then executes that code itself in the sandbox. `claude` is run headless
//! (`-p`), single-turn, with every agent tool disabled, so it behaves as a pure
//! text completion rather than an autonomous agent.

use std::pin::Pin;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use super::{CodeSink, CompletionRequest, LlmProvider, RawCompletion};

pub struct ClaudeCli;

impl LlmProvider for ClaudeCli {
    fn complete<'a>(
        &'a self,
        request: &'a CompletionRequest<'a>,
        sink: &'a mut dyn CodeSink,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<RawCompletion>> + 'a>> {
        Box::pin(run_claude(request, sink))
    }
}

/// The directory the `claude` child runs in: a neutral, empty "project" under
/// `~/.sdkmode`. The CLI injects per-project state — auto-memory, CLAUDE.md —
/// keyed to its working directory, and `--setting-sources`/
/// `--exclude-dynamic-system-prompt-sections` do not cover auto-memory. Run
/// in the user's real project directory, the engine inherits *their* memory
/// (names, notes, even memory-file instructions that fight the REPL's own
/// seed). An empty project keeps the completion pure. (`--bare` would also
/// skip auto-memory, but it restricts auth to API keys, breaking OAuth.)
fn engine_dir() -> std::path::PathBuf {
    let dir = crate::snapshot::dirs_home()
        .map(|mut home| {
            home.push(".sdkmode");
            home.push("engine");
            home
        })
        .unwrap_or_else(std::env::temp_dir);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Run `claude` once as a pure code-completion engine: its persona is fully
/// replaced by the request's system prompt, dynamic context (git status, env)
/// and project settings (CLAUDE.md, MCP servers) are excluded, auto-memory is
/// sidestepped by running in [`engine_dir`], and every built-in tool is off —
/// so it can only emit the next step's source. Streams text deltas to `sink`
/// and returns the full concatenated output plus the run's reported cost.
async fn run_claude(
    request: &CompletionRequest<'_>,
    sink: &mut dyn CodeSink,
) -> anyhow::Result<RawCompletion> {
    let mut child = Command::new("claude")
        .current_dir(engine_dir())
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
        .arg(request.system_prompt) // replace the persona entirely, not append
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
        stdin.write_all(request.context.as_bytes()).await?;
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

    Ok(RawCompletion {
        text: full,
        cost_usd,
    })
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

#[cfg(test)]
mod tests {
    use super::diagnose_failure;

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
}
