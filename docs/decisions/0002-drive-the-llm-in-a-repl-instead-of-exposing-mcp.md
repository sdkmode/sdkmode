---
status: "accepted"
date: 2026-06-28
decision-makers: Heidi Hill <heidi@heidi.codes>
consulted:
informed:
---

# Drive the LLM directly in a REPL instead of exposing the sandbox over MCP

## Context and Problem Statement

sdkmode is a sandbox that runs JavaScript against real, authenticated SDKs, with credentials injected by the host as requests leave the sandbox (see [ADR-0001](0001-build-sandbox-with-brokered-credential-injection.md)).
The original interface was an MCP server: an external agent connects over stdio and calls a `run_javascript` tool, one snippet per tool call.

How should the LLM interact with the sandbox?
Two shapes are possible: keep the LLM *outside* (an MCP client that calls `run_javascript` as one tool among many), or put the LLM *inside* the product — drive it ourselves in a read-eval-print loop where each turn the model writes JavaScript that we execute directly, with no MCP layer.

## Decision Drivers

* Latency and cost — every model round-trip is slow and paid for, so fewer is better.
* The sandbox's value (brokered auth, a persistent runtime, full npm/SDK access) is realised best when the model writes whole programs, not single tool calls.
* Use an interface the model already knows (plain JavaScript) rather than bespoke tool schemas it must be taught.
* Confidentiality — the model should never see credentials or the host environment.
* Ownership cost — how much agent machinery we have to build and maintain ourselves.

## Considered Options

* MCP server consumed by an external agent (the status quo)
* Embedded REPL: the LLM writes JavaScript, the host runs it directly, no MCP

## Decision Outcome

Chosen option: **Embedded REPL**, because it collapses multi-step API orchestration into a single program per step, which removes model round-trips and therefore wins decisively on latency and cost for tool-shaped tasks — while reusing the existing sandbox and credential broker unchanged.

The MCP server is **retained as a secondary interface** behind the `mcp` subcommand, so existing MCP clients (and the benchmark's comparison trial) keep working; the REPL is the default mode of the binary.

### Consequences

* Good, because multi-step tasks finish in far fewer model round-trips. On `repo-count-edit` the REPL completes in a single step (~9 s, ~$0.009) versus 4–6 turns and ~$0.05–0.12 for the MCP-based agents.
* Good, because the conversation history becomes a literal, growing JavaScript program, which is a coherent mental model for both the model and the reader.
* Good, because the model works in plain JavaScript — an interface it already knows — instead of having to learn custom tools.
* Good, because running the LLM inside the product let us lock the sandbox down to least privilege (no env, no subprocess, working-dir reads only, imports restricted to registered SDKs); credentials remain entirely host-side.
* Bad, because we now own the agent loop — prompting, step-vs-return semantics, output rendering, retries — instead of inheriting a mature agent harness's tool-use machinery.
* Bad, because the advantage is task-shaped: for simple read-then-summarise tasks the REPL only matches a no-tools baseline and trails it slightly on cost (`issue-summary`, n=10: REPL ~$0.047 vs no-tools ~$0.040), though it still beats the MCP agents.
* Neutral, because each step shells out to the `claude` CLI, adding a process dependency and per-step startup cost.

### Confirmation

`benchmark.py` runs the REPL (`sdkmode-repl`) head-to-head with the MCP trials (`sdkmode`, `github-mcp`) and a no-tools baseline (`no-mcp`) across a task set, reporting turns, duration, and cost.
The REPL path is exercised end-to-end by the `Session` and transform tests in the Rust suite.
Note the current benchmark measures cost and latency only, **not output quality** — see More Information.

## Pros and Cons of the Options

### MCP server consumed by an external agent

The sandbox is one tool (`run_javascript`) in an external agent's toolbelt.

* Good, because it reuses a mature agent harness (planning, tool routing, permissions) we don't have to build.
* Good, because it interoperates with any MCP-capable client.
* Neutral, because credential brokering works identically regardless of caller.
* Bad, because each `run_javascript` call is a separate model turn, so multi-step work pays repeated round-trip latency and cost.
* Bad, because the model treats the sandbox as a single function call rather than a place to compose whole programs, under-using its strengths.

### Embedded REPL (chosen)

We drive the model ourselves; each turn it writes JavaScript we execute in a persistent session, and a returned value ends the turn.

* Good, because whole-program steps minimise round-trips — the dominant cost and latency factor.
* Good, because state persists across steps, so the model builds on prior work instead of re-fetching.
* Good, because it enabled a strict, self-contained security posture.
* Bad, because we maintain the loop, prompt, and rendering ourselves.
* Bad, because we depend on the `claude` CLI as the completion engine.

## More Information

* Builds on [ADR-0001](0001-build-sandbox-with-brokered-credential-injection.md); the credential broker is unchanged by this decision.
* The MCP interface is not removed — it remains available via `sdkmode mcp` and is configured that way in `.mcp.json`.
* **Revisit trigger:** the benchmark does not yet evaluate output *quality*, only efficiency. A cheaper, faster answer that is worse is not a win. Before treating the cost/latency advantage as decisive, add quality evaluation — objective end-state checks for verifiable tasks, and a blind pairwise LLM judge for open-ended ones — and re-confirm the outcome.
