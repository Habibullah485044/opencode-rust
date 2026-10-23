# Benchmarks

Small tasks with hidden checks, to compare opencode-rust versions, models and
other tools on the same prompt.

| Task | Prompt | Scorer |
|---|---|---|
| inventory | Python CLI with tests | `inventory/score.py`: 12 hidden behaviours + its own tests, plus 3 "extra quality" checks |
| homepage | Plain HTML/CSS/JS homepage | `homepage/score.py`: structure and code, then a real browser at 360/768/1440 px |

Run opencode-rust on all tasks (fresh folder each, headless, every tool call
approved — only use it for these tasks):

```bash
python -I bench/run.py --model <model-id>
```

Each run appends a line to `bench/results.csv`: score, extra checks, seconds,
requests, input/cached/output tokens. To compare with another tool, paste the
prompt from the task's `PROMPT.md` into it in a new empty folder and run the
task's `score.py` on that folder.
