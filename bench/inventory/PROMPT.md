# Benchmark: inventory CLI

Paste the six lines below, unchanged, into each tool. Start each run in its own
new, empty folder, with the same model.

```
In this empty folder, build a Python package `inventory` that runs as `python -m inventory` and keeps its data in `inventory.json` in the current folder.
Commands: `add <sku> <qty>` and `remove <sku> <qty>`; qty must be a whole number above 0, otherwise print an error to stderr and exit with code 2.
Removing more than is in stock prints "not enough stock" to stderr, exits with code 1 and changes nothing.
`report` prints one line `<sku> <qty>` per item, sorted by sku, then a last line `TOTAL <sum>`; an item that reaches 0 is left out.
Write pytest tests in `tests/` that cover every command and every error case, and make sure they all pass.
Work in parallel: one agent writes the code, one writes the tests, one reviews both; end with a short summary of what you built.
```

## Scoring

After each run:

```bash
python -I bench/inventory/score.py <the folder the tool worked in>
```

It checks 12 behaviours against hidden cases (the tool never sees them), runs
the tool's own tests, and prints a score out of 100. Also note for each run:
the time it took, and the tokens or credits it used (`/usage` in opencode-rust).
