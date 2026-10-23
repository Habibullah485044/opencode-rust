"""Run opencode-rust on the benchmark tasks and score each run.

Usage:
    python -I bench/run.py [--bin PATH] [--model MODEL] [--tasks inventory,homepage]

For every task: a new empty folder, the task's 6-line prompt from
bench/<task>/PROMPT.md run headless (`--headless --yes`), then
bench/<task>/score.py on the result. Tokens come from the app's own usage
records for the chats the run created. One line per run is added to
bench/results.csv, so runs can be compared across versions and models.

Note: --yes approves every tool call. Run it only on these benchmark tasks.
"""

import argparse
import csv
import datetime
import os
import re
import sqlite3
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)


def prompt_of(task):
    text = open(os.path.join(HERE, task, "PROMPT.md"), encoding="utf-8").read()
    m = re.search(r"```\n(.*?)\n```", text, re.S)
    if not m:
        sys.exit(f"{task}: no fenced prompt in PROMPT.md")
    return m.group(1).strip()


def state_db():
    base = os.environ.get("APPDATA") or os.path.expanduser("~/.local/share")
    return os.path.join(base, "opencode-rust", "opencode.db")


def usage_since(db, since_ms):
    """Token totals of the chats created since `since_ms` (subagents included)."""
    if not os.path.isfile(db):
        return {}
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    row = con.execute(
        "SELECT COALESCE(SUM(requests),0), COALESCE(SUM(input),0), COALESCE(SUM(cached),0), COALESCE(SUM(output),0)"
        " FROM usage WHERE session_id IN (SELECT id FROM sessions WHERE created_at >= ?)",
        (since_ms,),
    ).fetchone()
    con.close()
    return dict(zip(("requests", "input", "cached", "output"), row))


def score(task, folder):
    out = subprocess.run([sys.executable, "-I", os.path.join(HERE, task, "score.py"), folder],
                         capture_output=True, text=True, encoding="utf-8", errors="replace")
    text = out.stdout + out.stderr
    m = re.search(r"SCORE (\d+)/100", text)
    extra = re.search(r"EXTRA (\d+)/(\d+)", text)
    return (int(m.group(1)) if m else None), (extra.group(0) if extra else ""), text


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.path.join(ROOT, "target", "release", "opencode-rust.exe" if os.name == "nt" else "opencode-rust"))
    ap.add_argument("--model", default=None)
    ap.add_argument("--tasks", default="inventory,homepage")
    ap.add_argument("--timeout", type=int, default=3600)
    args = ap.parse_args()
    exe = args.bin
    if not os.path.isfile(exe):
        alt = exe.replace(os.sep + "release" + os.sep, os.sep + "debug" + os.sep)
        exe = alt if os.path.isfile(alt) else sys.exit(f"no binary at {args.bin} (build it, or pass --bin)")
    results = os.path.join(HERE, "results.csv")
    new_file = not os.path.isfile(results)
    for task in [t.strip() for t in args.tasks.split(",") if t.strip()]:
        folder = tempfile.mkdtemp(prefix=f"bench-{task}-")
        cmd = [exe, "--headless", "--yes", "--timeout", str(args.timeout)]
        if args.model:
            cmd += ["--model", args.model]
        cmd += ["--", prompt_of(task)]
        print(f"== {task}: running in {folder}")
        since = int(time.time() * 1000) - 1000
        t0 = time.time()
        run = subprocess.run(cmd, cwd=folder, capture_output=True, text=True, encoding="utf-8", errors="replace")
        secs = round(time.time() - t0)
        points, extra, report = score(task, folder)
        use = usage_since(state_db(), since)
        print(report.strip())
        print(f"-> {task}: score {points}  {extra}  {secs}s  tokens in {use.get('input', 0)} "
              f"(cached {use.get('cached', 0)}) out {use.get('output', 0)}  requests {use.get('requests', 0)}  exit {run.returncode}")
        with open(results, "a", newline="", encoding="utf-8") as f:
            w = csv.writer(f)
            if new_file:
                w.writerow(["when", "task", "model", "score", "extra", "seconds", "requests", "input", "cached", "output", "exit", "folder"])
                new_file = False
            w.writerow([datetime.datetime.now().isoformat(timespec="seconds"), task, args.model or "(config)", points, extra,
                        secs, use.get("requests", 0), use.get("input", 0), use.get("cached", 0), use.get("output", 0),
                        run.returncode, folder])
    print(f"\nresults: {results}")


if __name__ == "__main__":
    main()
