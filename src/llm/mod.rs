//! The LLM engine layer: turns REPL context into the next step's JavaScript.
//!
//! Split in two: this module owns everything provider-independent — the system
//! prompt (the product's instruction contract), the retry loop, and the
//! code-extraction repairs — while an [`LlmProvider`] implementation owns only
//! how to obtain a raw completion (today: driving the `claude` CLI, see
//! [`claude_cli`]; a direct Anthropic-API provider would slot in beside it).
//!
//! The system prompt is deliberately *state on the engine*, not a global: it is
//! assembled from the SDK registry when the [`Llm`] is constructed and handed
//! to the provider on every request. Providers differ in how they deliver it
//! (a CLI flag, an API `system` parameter) but never in what it says.

use std::pin::Pin;

pub mod claude_cli;

/// Receives the model's streamed output so it can be rendered as it arrives.
pub trait CodeSink {
    /// A chunk of generated text just arrived.
    fn on_delta(&mut self, text: &str);
    /// The previous attempt failed; discard anything shown and reset.
    fn on_retry(&mut self);
}

/// One completion request, as handed to a provider: the full instruction
/// contract plus the rendered session program to complete.
pub struct CompletionRequest<'a> {
    /// The engine's system prompt (see [`build_system_prompt`]). The provider
    /// must deliver it as the model's *entire* persona — replacing, not
    /// appending to, whatever default persona the provider has.
    pub system_prompt: &'a str,
    /// The rendered session program (see `repl::build_context`), ending in the
    /// `prompt = "..."` line the model is asked to continue from.
    pub context: &'a str,
}

/// A provider's raw, unrepaired output for one request.
pub struct RawCompletion {
    /// The model's full reply text (possibly with fences/prose to repair).
    pub text: String,
    /// What this request cost, in USD, if the provider can report it.
    pub cost_usd: f64,
}

/// One way of obtaining completions. Implementations should be stateless per
/// request (any connection/process state is their own affair) and must stream
/// text to the sink as it arrives so the REPL can render it live.
pub trait LlmProvider {
    /// Run one completion, streaming deltas to `sink`. Errors are considered
    /// transient by the engine and retried once (see [`Llm::complete`]).
    fn complete<'a>(
        &'a self,
        request: &'a CompletionRequest<'a>,
        sink: &'a mut dyn CodeSink,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<RawCompletion>> + 'a>>;
}

/// Instruction layered onto the model so it answers with bare JavaScript: the
/// turn mechanics (fixed) — the Environment section is appended by
/// [`build_system_prompt`] from the SDK registry.
const PROMPT_CORE: &str = "\
You are the engine of a stateful, multi-step JavaScript REPL agent.

You receive the session so far as a JavaScript program, ending in a line \
`prompt = \"<the user's latest message>\";`. Reply with ONLY the JavaScript \
source for the NEXT step — no markdown, no code fences, no prose outside // \
comments. Plain English on its own line is a syntax error.

How a turn works:
- Each reply is one step that runs immediately. You then see its output and may \
write another step.
- Prefer variables over console.log, always. State persists across steps, so \
hold data in variables and let CODE make every decision code can make — max, \
sort, filter, count, and compare are computations, not judgement calls. \"What \
is my most-starred repo?\" is ONE step that computes the answer and returns it; \
logging the repo list first would be a wasted step and wasted tokens.
- console.log is ONLY for data you must exercise judgement on — a call code \
cannot make (funniest, most interesting, seems suspicious). Log the smallest \
projection that lets you judge, never whole objects: for \"the funniest repo \
name I starred\", log repos.map(r => r.name) — names only — end the step, read \
them, then return your pick. Never log data you are about to `return`, and if a \
line of code could have made the decision, logging it was a mistake.
- You never see a step's output until after the step runs. So when the answer \
needs your judgement over data — reading fetched page text, choosing among \
results, summarizing file contents — that step must STOP without returning: \
capture the data, log the minimal projection, end the step, actually read it, \
then answer on the NEXT step. Never write a comment guessing what the output \
will say, and never `return` an answer built from output you have not read \
back — you would be inventing it, and the run will show you were wrong. (When \
code alone computes the final answer, there is nothing to read: compute and \
return in the same step.)
- Split big jobs into small, recoverable steps: each step completes one unit of \
work whose result survives — in a variable or written to disk — so a failure \
costs one step, not the whole job. Building something multi-file (a game, a \
site)? Write ONE file per step; never emit every file in one giant step where a \
single error forces regenerating all of it. Likewise, build a large artifact — \
a file's contents, a big array — once, into a variable, and act on it (write, \
push, upload) in the NEXT step: if that step fails, the variable is still there \
to retry with — never rebuild it from scratch, and never re-emit or log the \
literal. But do not split what code finishes alone: fetch → compute → return is \
one step.
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
to exactly that, nothing extraneous.";

/// The full system prompt: [`PROMPT_CORE`] plus an Environment section built
/// from the SDK registry — the import allowlist sentence and each SDK's bullet
/// (see [`crate::sdk::Docs`]). Built when an [`Llm`] is constructed, so the
/// prompt has the engine's lifetime rather than the process's: if the registry
/// ever becomes configurable (per-project SDKs), each engine sees its own.
fn build_system_prompt() -> String {
    let sdks = crate::sdk::registry();
    let blurbs: Vec<&str> = sdks.iter().map(|sdk| sdk.docs().import_blurb).collect();

    let mut prompt = String::from(PROMPT_CORE);
    prompt.push_str(
        "\n\nEnvironment:\n\
         - State persists across steps and turns: variables, functions, and classes you \
         declare stay available. Build on them instead of recomputing.\n\
         - A sandboxed Deno runtime; top-level await is allowed. You may read files in \
         the working directory, but cannot access environment variables, spawn \
         processes, or reach private network hosts.\n\
         - Imports are bare specifiers (never URLs) from a small allowlist: ",
    );
    prompt.push_str(&crate::sdk::oxford_join(&blurbs));
    prompt.push_str(". Do not import anything else.");
    for sdk in &sdks {
        if let Some(bullet) = sdk.docs().system_prompt {
            prompt.push_str("\n- ");
            prompt.push_str(bullet);
        }
    }
    prompt
}

/// Extra attempts if the provider fails (transient cold-start failures).
const RETRIES: usize = 1;

/// One step's result: the extracted JavaScript and what the request cost.
pub struct Completion {
    pub code: String,
    pub cost_usd: f64,
}

/// The engine: a provider plus the system prompt it delivers. Construct one
/// per session (the REPL builds one in `run()`) and reuse it across turns.
pub struct Llm {
    provider: Box<dyn LlmProvider>,
    system_prompt: String,
}

impl Llm {
    /// The default engine: the `claude` CLI provider.
    pub fn new() -> Self {
        Self::with_provider(Box::new(claude_cli::ClaudeCli))
    }

    pub fn with_provider(provider: Box<dyn LlmProvider>) -> Self {
        Self {
            provider,
            system_prompt: build_system_prompt(),
        }
    }

    /// Ask the provider for the next step's JavaScript, streaming its output to
    /// `sink` and returning the extracted source plus the step's cost. Retries
    /// once on a transient failure.
    pub async fn complete(
        &self,
        context: &str,
        sink: &mut dyn CodeSink,
    ) -> anyhow::Result<Completion> {
        let request = CompletionRequest {
            system_prompt: &self.system_prompt,
            context,
        };

        let mut last_error = None;
        for attempt in 0..=RETRIES {
            if attempt > 0 {
                sink.on_retry();
            }
            match self.provider.complete(&request, &mut *sink).await {
                Ok(raw) => {
                    return Ok(Completion {
                        code: extract_code(&raw.text),
                        cost_usd: raw.cost_usd,
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
}

/// Turn the model's reply into runnable source, repairing the two ways the
/// model leaks non-code: a markdown code fence (even with prose before it), and
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
    use super::{build_system_prompt, extract_code};

    /// The assembled prompt must mention every *model-facing* import specifier
    /// (those with a non-empty `import_blurb`) and every SDK bullet, so the
    /// model can learn to use each capability. An SDK whose package is only
    /// imported by its own shim (e.g. discord/discordeno) carries an empty
    /// blurb and is documented through its bullet instead, not the allowlist.
    #[test]
    fn system_prompt_covers_every_sdk() {
        let prompt = build_system_prompt();
        for sdk in crate::sdk::registry() {
            let docs = sdk.docs();
            if !docs.import_blurb.is_empty() {
                for (specifier, _url) in sdk.imports() {
                    assert!(
                        prompt.contains(specifier),
                        "system prompt does not mention import {specifier:?} ({})",
                        sdk.name()
                    );
                }
            }
            if let Some(bullet) = docs.system_prompt {
                assert!(
                    prompt.contains(bullet),
                    "system prompt is missing the {} bullet",
                    sdk.name()
                );
            }
        }
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
