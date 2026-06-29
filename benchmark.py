#!/usr/bin/env python3
"""Benchmark a prompt against sdkmode (run_javascript) vs. the github MCP plugin.

Trials:
  - sdkmode      — Claude (via the agent SDK) using sdkmode's run_javascript MCP tool
  - github-mcp   — Claude using the official GitHub MCP plugin
  - no-mcp       — Claude with only its built-in tools
  - sdkmode-repl — the new approach: the sdkmode binary as its OWN agent (no MCP).
                   It reads the prompt on stdin, runs its multi-step REPL loop, and
                   prints the answer on stdout. Only wall-clock and success are
                   measured here (turn/cost metrics live inside the binary).

Prerequisites:
  - Python >= 3.10
  - pip install claude-agent-sdk
  - cargo build --release   (builds ./target/release/sdkmode)
  - gh auth login           (sdkmode brokers GitHub auth through the gh CLI)
  - the "github" Claude Code plugin enabled for this project
    (.claude/settings.local.json: enabledPlugins["github@claude-plugins-official"])

Usage:
  python3 benchmark.py ["custom prompt"]
"""

import argparse
import asyncio
import json
import os
import statistics
import time
from pathlib import Path

from claude_agent_sdk import ClaudeAgentOptions, ResultMessage, query

REPO_ROOT = Path(__file__).resolve().parent
SDKMODE_BIN = REPO_ROOT / "target" / "release" / "sdkmode"

# The sdkmode-repl trial drives its own multi-step agent loop, which can run for
# a while; cap it so a hung run can't stall the whole benchmark.
SDKMODE_REPL_TRIAL = "sdkmode-repl"
SDKMODE_REPL_TIMEOUT_S = 180.0

# sdkmode must only use its own tool; the github-mcp trial keeps its full
# normal toolset (including Bash/WebFetch fallback) since the point of this
# benchmark is sdkmode-only vs. a traditional/unrestricted MCP workflow.
SDKMODE_ONLY_BYPASS_TOOLS = ["Bash", "WebFetch", "WebSearch"]

# Each task prompt only ever edits issue #1's title/body in sdkmode/sdkmode
# (or reads a third-party issue) so repeats leave no clutter.
TASKS = {
    "issue-summary": (
        "Read issue #1683 in github/github-mcp-server, including its "
        "comments, and summarize what the discussion is about in a few "
        "sentences."
    ),
    "repo-count-edit": (
        "Edit the title of issue #1 in the sdkmode/sdkmode repository to "
        "include the total number of public repositories you own."
    ),
    "starred-repos-edit": (
        "Edit the description of issue #1 in the sdkmode/sdkmode repository "
        "to list the top 100 repos I've starred, ranked by star count."
    ),
}


def find_github_plugin_path() -> Path:
    """Locate the installed 'github' Claude Code plugin (its directory holds
    .mcp.json pointing at the official GitHub MCP server)."""
    plugins_root = Path.home() / ".claude" / "plugins"
    for manifest in plugins_root.glob("**/.claude-plugin/plugin.json"):
        try:
            data = json.loads(manifest.read_text())
        except (OSError, json.JSONDecodeError):
            continue
        plugin_dir = manifest.parent.parent
        if data.get("name") == "github" and (plugin_dir / ".mcp.json").exists():
            return plugin_dir
    raise SystemExit(
        f"Could not find the 'github' plugin under {plugins_root}. "
        "Install it with: /plugin install github@claude-plugins-official"
    )


def build_trials() -> dict[str, ClaudeAgentOptions]:
    return {
        "sdkmode": ClaudeAgentOptions(
            cwd=str(REPO_ROOT),
            setting_sources=[],
            mcp_servers={
                # The binary now defaults to the REPL; the MCP server is behind
                # the `mcp` subcommand.
                "sdkmode": {"type": "stdio", "command": str(SDKMODE_BIN), "args": ["mcp"]},
            },
            strict_mcp_config=True,
            disallowed_tools=SDKMODE_ONLY_BYPASS_TOOLS,
            permission_mode="bypassPermissions",
        ),
        "github-mcp": ClaudeAgentOptions(
            cwd=str(REPO_ROOT),
            setting_sources=[],
            mcp_servers={},
            strict_mcp_config=False,
            plugins=[{"type": "local", "path": str(find_github_plugin_path())}],
            permission_mode="bypassPermissions",
        ),
        # Status quo baseline: no GitHub-specific tooling at all, just the
        # default built-ins (Bash/gh CLI, WebFetch, WebSearch, etc.).
        "no-mcp": ClaudeAgentOptions(
            cwd=str(REPO_ROOT),
            setting_sources=[],
            mcp_servers={},
            strict_mcp_config=True,
            permission_mode="bypassPermissions",
        ),
    }


async def run_trial(name: str, prompt: str, options: ClaudeAgentOptions | None) -> dict:
    if name == SDKMODE_REPL_TRIAL:
        return await run_sdkmode_repl(prompt)

    result = None
    async for message in query(prompt=prompt, options=options):
        if isinstance(message, ResultMessage):
            result = message

    if result is None:
        raise RuntimeError(f"{name}: no result message received")

    usage = result.usage or {}
    return {
        "name": name,
        "is_error": result.is_error,
        "num_turns": result.num_turns,
        "duration_ms": result.duration_ms,
        "input_tokens": usage.get("input_tokens"),
        "output_tokens": usage.get("output_tokens"),
        "total_cost_usd": result.total_cost_usd,
        "result_text": result.result,
    }


async def run_sdkmode_repl(prompt: str) -> dict:
    """Run the new approach: the sdkmode binary as its own agent (no MCP).

    The binary reads the prompt on stdin, drives its own multi-step REPL loop
    (writing and running JavaScript in the sandbox), and prints the final answer
    on stdout. Per-turn and cost metrics live inside the binary and are not
    surfaced, so only wall-clock duration and success are reported here.
    """
    if not SDKMODE_BIN.exists():
        raise SystemExit(f"{SDKMODE_BIN} not found — run `cargo build --release` first.")

    start = time.monotonic()
    proc = await asyncio.create_subprocess_exec(
        str(SDKMODE_BIN),
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        cwd=str(REPO_ROOT),
        env={**os.environ, "SDKMODE_METRICS": "1"},
    )

    timed_out = False
    try:
        stdout, stderr = await asyncio.wait_for(
            proc.communicate(input=(prompt + "\n").encode()),
            timeout=SDKMODE_REPL_TIMEOUT_S,
        )
    except asyncio.TimeoutError:
        proc.kill()
        await proc.wait()
        stdout, stderr, timed_out = b"", b"", True

    duration_ms = (time.monotonic() - start) * 1000.0
    answer = stdout.decode(errors="replace").strip()
    is_error = timed_out or proc.returncode != 0 or not answer
    steps, cost = parse_sdkmode_metrics(stderr.decode(errors="replace"))

    return {
        "name": SDKMODE_REPL_TRIAL,
        "is_error": is_error,
        "num_turns": steps,
        "duration_ms": duration_ms,
        "input_tokens": None,
        "output_tokens": None,
        "total_cost_usd": cost,
        "result_text": answer,
    }


def parse_sdkmode_metrics(stderr: str) -> tuple[int | None, float | None]:
    """Extract (steps, cost_usd) from the `__sdkmode_metrics {json}` line the
    binary prints on stderr under SDKMODE_METRICS. Returns (None, None) if absent."""
    for line in reversed(stderr.splitlines()):
        marker = "__sdkmode_metrics "
        if line.startswith(marker):
            try:
                data = json.loads(line[len(marker):])
                return data.get("steps"), data.get("cost_usd")
            except json.JSONDecodeError:
                return None, None
    return None, None


def mean_stdev(values: list) -> tuple[float | None, float | None]:
    values = [v for v in values if v is not None]
    if not values:
        return None, None
    return statistics.mean(values), statistics.stdev(values) if len(values) > 1 else 0.0


def print_table(columns: list[str], rows: list[list[str]]) -> None:
    widths = [max(len(columns[i]), *(len(r[i]) for r in rows)) for i in range(len(columns))]

    def fmt(values: list) -> str:
        return "  ".join(str(v).ljust(w) for v, w in zip(values, widths))

    print(fmt(columns))
    print(fmt(["-" * w for w in widths]))
    for r in rows:
        print(fmt(r))


def fmt_stat(mean: float | None, sd: float | None, prec: int, prefix: str = "") -> str:
    """Format a mean±sd cell, or 'n/a' when the metric wasn't measured."""
    if mean is None:
        return "n/a"
    return f"{prefix}{mean:.{prec}f}±{prefix}{sd:.{prec}f}"


def print_variance_report(runs: dict[str, dict[str, list[dict]]]) -> None:
    columns = ["task", "trial", "errors", "turns (mean±sd)", "duration_ms (mean±sd)", "cost_usd (mean±sd)"]
    rows = []
    for task_name, trials in runs.items():
        for trial_name, results in trials.items():
            errors = sum(1 for r in results if r["is_error"])
            turns_mean, turns_sd = mean_stdev([r["num_turns"] for r in results])
            dur_mean, dur_sd = mean_stdev([r["duration_ms"] for r in results])
            cost_mean, cost_sd = mean_stdev([r["total_cost_usd"] for r in results])
            rows.append([
                task_name, trial_name, f"{errors}/{len(results)}",
                fmt_stat(turns_mean, turns_sd, 1),
                fmt_stat(dur_mean, dur_sd, 0),
                fmt_stat(cost_mean, cost_sd, 3, "$"),
            ])
    print_table(columns, rows)


async def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repeats", type=int, default=5,
        help="Number of times to repeat each (task, trial) combination (default: 5).",
    )
    parser.add_argument(
        "--concurrency", type=int, default=4,
        help="Max runs executing at once (default: 4). Use 1 for sequential.",
    )
    parser.add_argument(
        "--task", action="append", choices=list(TASKS),
        help="Limit to specific built-in task(s); repeatable. Default: run all.",
    )
    parser.add_argument(
        "prompt", nargs="?",
        help="Run a single ad-hoc prompt instead of the built-in task set.",
    )
    args = parser.parse_args()

    tasks = {"custom": args.prompt} if args.prompt else {
        name: TASKS[name] for name in (args.task or TASKS)
    }
    trials: dict[str, ClaudeAgentOptions | None] = build_trials()
    # The new approach is not an agent-SDK config; it runs the sdkmode binary.
    trials[SDKMODE_REPL_TRIAL] = None
    runs: dict[str, dict[str, list[dict]]] = {task: {trial: [] for trial in trials} for task in tasks}

    # Bound how many runs execute at once: each run spawns its own claude
    # session, so unbounded parallelism would swamp rate limits.
    semaphore = asyncio.Semaphore(max(1, args.concurrency))

    async def one_run(task_name: str, prompt: str, trial_name: str, options, i: int) -> None:
        async with semaphore:
            result = await run_trial(trial_name, prompt, options)
        # Appends within an asyncio task are safe (no await between read+write).
        runs[task_name][trial_name].append(result)
        turns = result["num_turns"] if result["num_turns"] is not None else "n/a"
        cost = (
            f"{result['total_cost_usd']:.3f}"
            if result["total_cost_usd"] is not None
            else "n/a"
        )
        print(
            f"{task_name} / {trial_name} (run {i + 1}/{args.repeats}): "
            f"turns={turns} duration_ms={result['duration_ms']:.0f} "
            f"cost_usd={cost} error={result['is_error']}"
        )

    jobs = [
        one_run(task_name, prompt, trial_name, options, i)
        for task_name, prompt in tasks.items()
        for trial_name, options in trials.items()
        for i in range(args.repeats)
    ]
    await asyncio.gather(*jobs)

    print("\n=== variance summary ===")
    print_variance_report(runs)


if __name__ == "__main__":
    asyncio.run(main())
