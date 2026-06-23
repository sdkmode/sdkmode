#!/usr/bin/env python3
"""Benchmark a prompt against sdkmode (run_javascript) vs. the github MCP plugin.

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
import statistics
from pathlib import Path

from claude_agent_sdk import ClaudeAgentOptions, ResultMessage, query

REPO_ROOT = Path(__file__).resolve().parent
SDKMODE_BIN = REPO_ROOT / "target" / "release" / "sdkmode"

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
                "sdkmode": {"type": "stdio", "command": str(SDKMODE_BIN)},
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


async def run_trial(name: str, prompt: str, options: ClaudeAgentOptions) -> dict:
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
                f"{turns_mean:.1f}±{turns_sd:.1f}",
                f"{dur_mean:.0f}±{dur_sd:.0f}",
                f"${cost_mean:.3f}±${cost_sd:.3f}",
            ])
    print_table(columns, rows)


async def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repeats", type=int, default=5,
        help="Number of times to repeat each (task, trial) combination (default: 5).",
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
    trials = build_trials()
    runs: dict[str, dict[str, list[dict]]] = {task: {trial: [] for trial in trials} for task in tasks}

    for task_name, prompt in tasks.items():
        for trial_name, options in trials.items():
            for i in range(args.repeats):
                result = await run_trial(trial_name, prompt, options)
                runs[task_name][trial_name].append(result)
                print(
                    f"{task_name} / {trial_name} (run {i + 1}/{args.repeats}): "
                    f"turns={result['num_turns']} duration_ms={result['duration_ms']} "
                    f"cost_usd={result['total_cost_usd']:.3f} error={result['is_error']}"
                )

    print("\n=== variance summary ===")
    print_variance_report(runs)


if __name__ == "__main__":
    asyncio.run(main())
