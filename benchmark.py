#!/usr/bin/env python3
"""Benchmark sdkmode against vanilla Claude Code on cost, speed, and correctness.

Both trials drive the same `claude` engine — `sdkmode` wraps it in a sandboxed
JavaScript REPL, `claude-code` runs it directly with its normal tools — so the
comparison isolates what the sandbox / code-as-action approach actually adds.

Every task has an objectively verifiable answer, checked live against the GitHub
API (via `gh`) at run time, so correctness is a real pass/fail, not vibes.

Prerequisites:
  - Python >= 3.10
  - claude (Claude Code CLI), authenticated
  - gh (GitHub CLI), authenticated  # used by the tasks AND to compute ground truth
  - cargo build --release           # builds ./target/release/sdkmode

Usage:
  python3 benchmark.py [--repeats N] [--concurrency N] [--task NAME ...]
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import re
import shutil
import statistics
import subprocess
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

REPO_ROOT = Path(__file__).resolve().parent
SDKMODE_BIN = REPO_ROOT / "target" / "release" / "sdkmode"
TIMEOUT_S = 180.0
# Throwaway repo for mutation tasks (created in the sdkmode org for this).
BENCHMARK_REPO = os.environ.get("BENCHMARK_REPO", "sdkmode/benchmark")


# --- ground truth helpers (via the gh CLI) --------------------------------

def gh(path: str, jq: str = ".", paginate: bool = False) -> str:
    cmd = ["gh", "api", path, "--jq", jq]
    if paginate:
        cmd.insert(2, "--paginate")
    out = subprocess.run(cmd, capture_output=True, text=True)
    if out.returncode != 0:
        raise RuntimeError(f"gh api {path} failed: {out.stderr.strip()}")
    return out.stdout.strip()


def gh_create_issue(repo: str, title: str, body: str) -> int:
    out = subprocess.run(
        ["gh", "api", "-X", "POST", f"repos/{repo}/issues",
         "-f", f"title={title}", "-f", f"body={body}", "--jq", ".number"],
        capture_output=True, text=True,
    )
    if out.returncode != 0:
        raise RuntimeError(f"create issue failed: {out.stderr.strip()}")
    return int(out.stdout.strip())


def gh_close_issue(repo: str, number: int) -> None:
    subprocess.run(
        ["gh", "api", "-X", "PATCH", f"repos/{repo}/issues/{number}", "-f", "state=closed"],
        capture_output=True, text=True,
    )


def owned_public_repos(jq_value: str) -> list[str]:
    """One `jq_value` per owned public repo, across all pages (aggregating in
    Python so pagination doesn't break per-page jq)."""
    out = gh(
        "user/repos?per_page=100&type=owner",
        f".[] | select(.private == false) | {jq_value}",
        paginate=True,
    )
    return [line for line in out.splitlines() if line.strip()]


def total_stars() -> str:
    return str(sum(int(n) for n in owned_public_repos(".stargazers_count")))


def repo_with_most_issues() -> str:
    rows = (line.split("\t") for line in owned_public_repos('"\\(.name)\t\\(.open_issues_count)"'))
    return max(rows, key=lambda r: int(r[1]))[0]


def recent_repos_with_issues() -> str:
    counts = gh(
        "user/repos?per_page=5&type=owner&sort=pushed&direction=desc",
        ".[] | .open_issues_count",
    )
    return str(sum(1 for n in counts.splitlines() if int(n) > 0))


def _starred_by_stars() -> list[str]:
    """All starred repos as 'full_name', sorted by star count (desc)."""
    out = gh(
        "user/starred?per_page=100",
        '.[] | "\\(.stargazers_count)\t\\(.full_name)"',
        paginate=True,
    )
    rows = [line.split("\t", 1) for line in out.splitlines() if line]
    rows.sort(key=lambda r: int(r[0]), reverse=True)
    return [name for _stars, name in rows]


def top_starred_repo() -> str:
    return _starred_by_stars()[0]


def starred_top_100() -> str:
    return "\n".join(_starred_by_stars()[:100])


def newest_repo() -> str:
    return gh("user/repos?per_page=1&type=owner&sort=created&direction=desc", ".[0].name")


def rust_file_count() -> str:
    return str(sum(1 for _ in (REPO_ROOT / "src").rglob("*.rs")))


def crate_version() -> str:
    match = re.search(r'(?m)^version\s*=\s*"([^"]+)"', (REPO_ROOT / "Cargo.toml").read_text())
    return match.group(1) if match else ""


# --- effect tasks: per-run setup/verify/teardown (mutations, local dev) ----

def setup_issue() -> dict:
    number = gh_create_issue(BENCHMARK_REPO, "benchmark: starred list", "(to be filled)")
    return {"repo": BENCHMARK_REPO, "issue": number}


def check_issue_starred(ctx: dict, _answer: str) -> bool:
    body = gh(f"repos/{ctx['repo']}/issues/{ctx['issue']}", ".body")
    expected = starred_top_100().splitlines()
    low = (body or "").lower()
    return bool(expected) and all(name.lower() in low for name in expected)


def teardown_issue(ctx: dict) -> None:
    gh_close_issue(ctx["repo"], ctx["issue"])


def setup_workdir() -> dict:
    tmp = tempfile.mkdtemp(prefix="sdkmode-bench-")
    (Path(tmp) / "demo.rs").write_text("// demo module\nfn existing() -> i32 { 1 }\n")
    return {"cwd": tmp}


def check_workdir(ctx: dict, _answer: str) -> bool:
    text = (Path(ctx["cwd"]) / "demo.rs").read_text()
    # Added the requested function and kept the existing code.
    return re.search(r"\bfn\s+doubled\b", text) is not None and "fn existing" in text


def teardown_workdir(ctx: dict) -> None:
    shutil.rmtree(ctx["cwd"], ignore_errors=True)


# --- tasks -----------------------------------------------------------------

@dataclass
class Task:
    prompt: str
    # Answer tasks: check the agent's reply against live ground truth.
    truth: Callable[[], str] | None = None
    kind: str = "text"  # "number" | "text" | "list"
    # Effect tasks: set up a per-run target, check the side effect, tear down.
    setup: Callable[[], dict] | None = None
    check: Callable[[dict, str], bool] | None = None  # (ctx, answer) -> correct
    teardown: Callable[[dict], None] | None = None
    cwd: Callable[[dict], str] | None = None  # per-run working dir (local dev)


TASKS: dict[str, Task] = {
    # Simple single-fact lookups.
    "open-issues": Task(
        prompt="How many open issues and open pull requests does the cli/cli repository have, in total?",
        truth=lambda: gh("repos/cli/cli", ".open_issues_count"),
        kind="number",
    ),
    "default-branch": Task(
        prompt="What is the default branch of the torvalds/linux repository?",
        truth=lambda: gh("repos/torvalds/linux", ".default_branch"),
        kind="text",
    ),
    "latest-release": Task(
        prompt="What is the tag name of the latest release of the cli/cli repository?",
        truth=lambda: gh("repos/cli/cli/releases/latest", ".tag_name"),
        kind="text",
    ),
    "license": Task(
        prompt="What is the SPDX license identifier of the facebook/react repository?",
        truth=lambda: gh("repos/facebook/react", ".license.spdx_id"),
        kind="text",
    ),
    # Spicier: multi-step aggregations over your own account.
    "total-stars": Task(
        prompt="What is the total number of stars across all public repositories you own on GitHub?",
        truth=total_stars,
        kind="number",
    ),
    "most-issues-repo": Task(
        prompt="Among the public repositories you own, which one has the most open issues and pull requests combined? Reply with the repository name.",
        truth=repo_with_most_issues,
        kind="text",
    ),
    "recent-with-issues": Task(
        prompt="Of your 5 most recently pushed repositories, how many have at least one open issue or pull request?",
        truth=recent_repos_with_issues,
        kind="number",
    ),
    # Multi-step parsing over data.
    "top-starred-repo": Task(
        prompt="Among the repositories you have starred on GitHub, which one has the most stars? Reply with its full name (owner/repo).",
        truth=top_starred_repo,
        kind="text",
    ),
    "newest-repo": Task(
        prompt="What is the name of the most recently created repository in your personal GitHub account (not counting organization repositories)?",
        truth=newest_repo,
        kind="text",
    ),
    # Data-movement: the answer is a large list. sdkmode returns it from a
    # variable (the runtime moves the data); claude-code must emit every line.
    "starred-top-100": Task(
        prompt="List your 100 most-starred starred repositories, ordered by star count — one per line, each as its full name in owner/repo form.",
        truth=starred_top_100,
        kind="list",
    ),
    # Local file reading.
    "rust-files": Task(
        prompt="How many Rust source files (ending in .rs) are in the src/ directory of this project, including subdirectories?",
        truth=rust_file_count,
        kind="number",
    ),
    "crate-version": Task(
        prompt="What is the package version declared under [package] in the Cargo.toml of this project?",
        truth=crate_version,
        kind="text",
    ),
    # Mutation + data-movement: sdkmode writes the list from a variable into the
    # issue body; claude-code must serialize all 100 lines into its edit.
    "issue-starred-100": Task(
        prompt="Edit the body of issue #{issue} in the {repo} repository so it lists your 100 most-starred starred repositories, ordered by star count — one per line, each as its full name in owner/repo form.",
        setup=setup_issue,
        check=check_issue_starred,
        teardown=teardown_issue,
    ),
    # Local agentic dev: edit a file in an isolated working copy.
    "local-add-fn": Task(
        prompt="In the file demo.rs in the current directory, add a Rust function `fn doubled(x: i32) -> i32` that returns x * 2. Keep the existing code intact.",
        setup=setup_workdir,
        check=check_workdir,
        teardown=teardown_workdir,
        cwd=lambda ctx: ctx["cwd"],
    ),
}


def verify(answer: str, truth: str, kind: str) -> bool:
    if kind == "number":
        found = {int(m.replace(",", "")) for m in re.findall(r"\d[\d,]*", answer)}
        return int(truth) in found
    if kind == "list":
        items = [i for i in truth.splitlines() if i.strip()]
        low = answer.lower()
        return bool(items) and all(i.lower() in low for i in items)
    return truth.lower() in answer.lower()


# --- trial runners (both shell out to `claude`) ----------------------------

def parse_metrics(stderr: str) -> float | None:
    for line in reversed(stderr.splitlines()):
        marker = "__sdkmode_metrics "
        if line.startswith(marker):
            try:
                return json.loads(line[len(marker):]).get("cost_usd")
            except json.JSONDecodeError:
                return None
    return None


async def run_sdkmode(prompt: str, cwd: str | None = None) -> dict:
    if not SDKMODE_BIN.exists():
        raise SystemExit(f"{SDKMODE_BIN} not found — run `cargo build --release` first.")
    start = time.monotonic()
    proc = await asyncio.create_subprocess_exec(
        str(SDKMODE_BIN),
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        cwd=cwd or str(REPO_ROOT),
        env={**os.environ, "SDKMODE_METRICS": "1"},
    )
    try:
        stdout, stderr = await asyncio.wait_for(
            proc.communicate(input=(prompt + "\n").encode()), TIMEOUT_S
        )
        timed_out = False
    except asyncio.TimeoutError:
        proc.kill()
        await proc.wait()
        stdout, stderr, timed_out = b"", b"", True

    answer = stdout.decode(errors="replace").strip()
    return {
        "answer": answer,
        "cost": parse_metrics(stderr.decode(errors="replace")),
        "duration_s": time.monotonic() - start,
        "error": timed_out or proc.returncode != 0 or not answer,
    }


async def run_claude_code(prompt: str, cwd: str | None = None) -> dict:
    start = time.monotonic()
    proc = await asyncio.create_subprocess_exec(
        "claude", "-p", prompt,
        "--permission-mode", "bypassPermissions",  # let it use Bash/gh without prompting
        "--setting-sources", "",                    # vanilla: no project CLAUDE.md/settings
        "--output-format", "json",
        stdin=asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        cwd=cwd or str(REPO_ROOT),
    )
    try:
        stdout, _stderr = await asyncio.wait_for(proc.communicate(), TIMEOUT_S)
        timed_out = False
    except asyncio.TimeoutError:
        proc.kill()
        await proc.wait()
        stdout, timed_out = b"", True

    answer, cost, error = "", None, timed_out or proc.returncode != 0
    if not timed_out and stdout:
        try:
            data = json.loads(stdout.decode(errors="replace"))
            answer = (data.get("result") or "").strip()
            cost = data.get("total_cost_usd")
            error = error or bool(data.get("is_error")) or not answer
        except json.JSONDecodeError:
            error = True
    return {"answer": answer, "cost": cost, "duration_s": time.monotonic() - start, "error": error}


TRIALS: dict[str, Callable] = {
    "sdkmode": run_sdkmode,
    "claude-code": run_claude_code,
}


# --- reporting -------------------------------------------------------------

def mean_stdev(values: list) -> tuple[float | None, float | None]:
    values = [v for v in values if v is not None]
    if not values:
        return None, None
    return statistics.mean(values), statistics.stdev(values) if len(values) > 1 else 0.0


def fmt_stat(mean: float | None, sd: float | None, prec: int, prefix: str = "") -> str:
    if mean is None:
        return "n/a"
    return f"{prefix}{mean:.{prec}f}±{prefix}{sd:.{prec}f}"


def print_table(columns: list[str], rows: list[list[str]]) -> None:
    widths = [max(len(columns[i]), *(len(r[i]) for r in rows)) for i in range(len(columns))]

    def fmt(values: list) -> str:
        return "  ".join(str(v).ljust(w) for v, w in zip(values, widths))

    print(fmt(columns))
    print(fmt(["-" * w for w in widths]))
    for r in rows:
        print(fmt(r))


def print_report(runs: dict[str, dict[str, list[dict]]]) -> None:
    columns = ["task", "trial", "correct", "cost_usd (mean±sd)", "duration_s (mean±sd)"]
    rows = []
    for task_name, trials in runs.items():
        for trial_name, results in trials.items():
            n = len(results)
            correct = sum(1 for r in results if r["correct"])
            cost_mean, cost_sd = mean_stdev([r["cost"] for r in results])
            dur_mean, dur_sd = mean_stdev([r["duration_s"] for r in results])
            rows.append([
                task_name, trial_name, f"{correct}/{n}",
                fmt_stat(cost_mean, cost_sd, 3, "$"),
                fmt_stat(dur_mean, dur_sd, 1),
            ])
    print_table(columns, rows)


# --- main ------------------------------------------------------------------

async def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repeats", type=int, default=5,
                        help="Repeats per (task, trial) (default: 5).")
    parser.add_argument("--concurrency", type=int, default=4,
                        help="Max runs at once (default: 4). Use 1 for sequential.")
    parser.add_argument("--task", action="append", choices=list(TASKS),
                        help="Limit to specific task(s); repeatable. Default: all.")
    args = parser.parse_args()

    tasks = {name: TASKS[name] for name in (args.task or TASKS)}
    runs: dict[str, dict[str, list[dict]]] = {
        t: {trial: [] for trial in TRIALS} for t in tasks
    }
    semaphore = asyncio.Semaphore(max(1, args.concurrency))

    async def one_run(task_name: str, task: Task, trial_name: str, runner: Callable, i: int) -> None:
        async with semaphore:
            # Per-run setup (create the issue / temp workdir), if any.
            try:
                ctx = await asyncio.to_thread(task.setup) if task.setup else {}
            except Exception as error:
                print(f"{task_name} / {trial_name} ({i + 1}/{args.repeats}): setup failed: {error}")
                return
            try:
                prompt = task.prompt.format(**ctx) if ctx else task.prompt
                cwd = task.cwd(ctx) if task.cwd else None
                result = await runner(prompt, cwd)
                correct = False
                if not result["error"]:
                    try:
                        if task.check:
                            correct = await asyncio.to_thread(task.check, ctx, result["answer"])
                        else:
                            truth = await asyncio.to_thread(task.truth)
                            correct = verify(result["answer"], truth, task.kind)
                    except Exception:
                        correct = False  # verification failed; can't credit it
            finally:
                if task.teardown:
                    try:
                        await asyncio.to_thread(task.teardown, ctx)
                    except Exception:
                        pass

        result["correct"] = correct
        runs[task_name][trial_name].append(result)
        cost = f"${result['cost']:.3f}" if result["cost"] is not None else "n/a"
        print(
            f"{task_name} / {trial_name} ({i + 1}/{args.repeats}): "
            f"correct={correct} cost={cost} {result['duration_s']:.1f}s error={result['error']}"
        )

    jobs = [
        one_run(task_name, task, trial_name, runner, i)
        for task_name, task in tasks.items()
        for trial_name, runner in TRIALS.items()
        for i in range(args.repeats)
    ]
    await asyncio.gather(*jobs)

    print("\n=== summary ===")
    print_report(runs)


if __name__ == "__main__":
    asyncio.run(main())
