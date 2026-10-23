"""Score one run of the inventory benchmark (see PROMPT.md).

Usage: python -I score.py <folder the tool worked in>

Hidden cases run in fresh empty folders with the solution on PYTHONPATH, so
leftover data in the solution folder cannot help or hurt. Score out of 100:
72 for the 12 behaviours, 20 when the solution's own tests pass, 8 for how
many tests it wrote.
"""

import os
import re
import shutil
import subprocess
import sys
import tempfile

TIMEOUT = 30


def run(sol, cwd, *args):
    env = dict(os.environ, PYTHONPATH=sol, PYTHONDONTWRITEBYTECODE="1")
    try:
        p = subprocess.run(
            [sys.executable, "-m", "inventory", *args],
            cwd=cwd, env=env, capture_output=True, text=True, timeout=TIMEOUT,
        )
        return p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired:
        return None, "", "timed out"


def lines(text):
    return [l.strip() for l in text.strip().splitlines() if l.strip()]


def case(sol, steps):
    """Run steps in a fresh folder; return the result of the last one."""
    cwd = tempfile.mkdtemp(prefix="inv-case-")
    out = None
    for args in steps:
        out = run(sol, cwd, *args)
    return out, cwd


def check_all(sol):
    results = []

    def check(name, ok, got=""):
        results.append((name, bool(ok), got))

    (code, out, err), _ = case(sol, [["add", "b", "3"], ["add", "a", "5"], ["add", "a", "2"], ["report"]])
    check("add then report: sorted, summed, TOTAL", code == 0 and lines(out) == ["a 7", "b 3", "TOTAL 10"], out)

    (code, out, err), _ = case(sol, [["report"]])
    check("report on an empty store prints TOTAL 0", code == 0 and lines(out) == ["TOTAL 0"], out + err)

    (code, out, err), cwd = case(sol, [["add", "a", "4"]])
    check("data is kept in inventory.json in the current folder",
          code == 0 and os.path.isfile(os.path.join(cwd, "inventory.json")), os.listdir(cwd))

    (code, out, err), _ = case(sol, [["add", "a", "4"], ["remove", "a", "1"], ["report"]])
    check("remove lowers the stock", code == 0 and lines(out) == ["a 3", "TOTAL 3"], out)

    (code, out, err), _ = case(sol, [["add", "a", "4"], ["add", "b", "1"], ["remove", "a", "4"], ["report"]])
    check("an item at 0 is left out of the report", code == 0 and lines(out) == ["b 1", "TOTAL 1"], out)

    (code, out, err), _ = case(sol, [["add", "a", "2"], ["remove", "a", "3"]])
    check("removing too much exits 1", code == 1, code)
    check("removing too much says 'not enough stock' on stderr", "not enough stock" in err.lower(), err)

    (code, out, err), _ = case(sol, [["add", "a", "2"], ["remove", "a", "3"], ["report"]])
    check("removing too much changes nothing", lines(out) == ["a 2", "TOTAL 2"], out)

    (code, out, err), _ = case(sol, [["remove", "zz", "1"]])
    check("removing an unknown sku exits 1", code == 1, code)

    bad = []
    for args in (["add", "a", "0"], ["add", "a", "-3"], ["add", "a", "abc"], ["add", "a", "1.5"], ["remove", "a", "0"]):
        (code, out, err), _ = case(sol, [args])
        bad.append((args, code, bool(err.strip())))
    check("a qty that is not a whole number above 0 exits 2 with a message on stderr",
          all(c == 2 and e for _, c, e in bad), bad)

    (code, out, err), _ = case(sol, [["add", "a", "0"], ["report"]])
    check("a rejected add changes nothing", lines(out) == ["TOTAL 0"], out)

    (code, out, err), _ = case(sol, [["add", "Widget-9", "12"], ["add", "apple", "1"], ["report"]])
    check("skus are kept as typed and sorted by text",
          code == 0 and lines(out) == ["Widget-9 12", "apple 1", "TOTAL 13"], out)
    return results


def own_tests(sol):
    work = tempfile.mkdtemp(prefix="inv-own-")
    copy = os.path.join(work, "sol")
    shutil.copytree(sol, copy, ignore=shutil.ignore_patterns(".git", "__pycache__", ".pytest_cache", "inventory.json"))
    env = dict(os.environ, PYTHONPATH=copy, PYTHONDONTWRITEBYTECODE="1")
    try:
        p = subprocess.run([sys.executable, "-m", "pytest", "-q", "-p", "no:cacheprovider", "tests"],
                           cwd=copy, env=env, capture_output=True, text=True, timeout=300)
    except subprocess.TimeoutExpired:
        return False, 0, "pytest timed out"
    summary = (p.stdout.strip().splitlines() or [""])[-1]
    passed = sum(int(n) for n in re.findall(r"(\d+) passed", summary))
    return p.returncode == 0, passed, summary


def main():
    if len(sys.argv) != 2 or not os.path.isdir(sys.argv[1]):
        sys.exit("usage: python -I score.py <solution folder>")
    sol = os.path.abspath(sys.argv[1])
    results = check_all(sol)
    score = 0
    for name, ok, got in results:
        score += 6 if ok else 0
        print(("PASS " if ok else "FAIL ") + name + ("" if ok else f"   got: {got!r}"[:200]))
    green, passed, summary = own_tests(sol)
    print(("PASS " if green else "FAIL ") + f"its own tests pass   ({summary})")
    score += 20 if green else 0
    bonus = 8 if passed >= 8 else 4 if passed >= 4 else 0
    print(f"tests written: {passed} passing  (+{bonus})")
    score += bonus
    print(f"\nSCORE {score}/100   behaviours {sum(ok for _, ok, _ in results)}/{len(results)}")
    extra = extra_quality(sol)
    print("\nExtra quality (not asked in the prompt; not part of the score):")
    for name, ok, got in extra:
        print(("  PASS " if ok else "  FAIL ") + name + ("" if ok else f"   got: {ascii(got)}"[:160]))
    print(f"EXTRA {sum(ok for _, ok, _ in extra)}/{len(extra)}")


# Root files a finished project may have besides the package and its tests.
ALLOWED_ROOT = {"README.md", "README", "pyproject.toml", "setup.py", "setup.cfg", "requirements.txt",
                "conftest.py", "pytest.ini", "tox.ini", "LICENSE", ".gitignore"}


def extra_quality(sol):
    """What a careful senior developer does without being asked."""
    out = []
    cwd = tempfile.mkdtemp(prefix="inv-extra-")
    junk = "{not json"
    with open(os.path.join(cwd, "inventory.json"), "w", encoding="utf-8") as f:
        f.write(junk)
    run(sol, cwd, "add", "a", "1")
    with open(os.path.join(cwd, "inventory.json"), encoding="utf-8") as f:
        kept = f.read() == junk
    out.append(("a corrupt inventory.json is not overwritten (no silent data loss)", kept, "file was replaced"))

    loose = []
    for q in ("+5", " 5", "1_000", "٣"):  # the last is an Arabic-Indic 3
        (code, _, _), _ = case(sol, [["add", "a", q]])
        loose.append((q, code))
    out.append(("quantities with a sign, spaces, underscores or non-ASCII digits are refused",
                all(c == 2 for _, c in loose), loose))

    extra_files = [n for n in os.listdir(sol)
                   if not n.startswith(".") and n not in ALLOWED_ROOT
                   and n not in ("inventory", "tests") and n != "__pycache__"]
    out.append(("no leftover scratch files or test data in the project", not extra_files, extra_files))
    return out


if __name__ == "__main__":
    main()
