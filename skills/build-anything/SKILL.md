---
name: build-anything
description: For any goal bigger than one edit (websites, features, bugs, research, docs) - split into small checked subtasks, then combine. Also copying the user's own site, code review, reverse engineering.
---

# Build anything

One method for every goal: **decompose → do the smallest pieces → check each →
compose → check the whole.** It is the idea of functional programming applied to
work: a big function is written as small functions, each taking a clear input and
returning a clear output, and the result is their composition. Each small piece
can be done, checked and redone on its own, so one mistake never spoils the rest.

Read only what the task needs. This file is the method. The `references/` folder
holds the detail, opened when a step calls for it:

| When | Read |
|---|---|
| Planning any goal with more than 2–3 steps | `references/decompose.md` |
| Building or changing a website or web page | `references/website.md` |
| The user wants a copy of a website | `references/copy-own-site.md` (ownership first) |
| Code review, PR splitting, commit messages, hand-offs, subagent decisions, long commands or sessions | `references/catalog.md` → the MCode Stack skill it names |
| How a shipped app, binary, extension or packaged site works inside | `references/catalog.md` → REA |

## 1. Understand the goal (one minute)

Write the goal as one sentence with a **done-when** test:
"Done when: the page opens on a phone, the menu works, and check_page passes."
If you cannot write the done-when test, the goal is unclear: make the most
reasonable assumption, say it in one line, and go on. Ask the user only when two
readings would lead to very different work (with `ask_user`, giving choices).

Look before planning: `list` the folder, `read` the files the goal touches, `grep`
for the names involved. Plans made without looking are wrong.

## 2. Decompose into small functions

Break the goal into subtasks. Each subtask is written like a function:

```
name:    add_menu_toggle
input:   index.html header, script.js
output:  a <button aria-expanded> that shows/hides #nav on phones
check:   check_page at 375px: menu opens and closes
depends: build_header
```

Rules (details and examples in `references/decompose.md`):
- **Small:** one subtask = one focused change you can finish and check in a few
  tool calls. If it needs more than ~5 files or ~150 lines, split it again.
- **Clear input and output:** say what it reads and what it produces. A subtask
  that "improves things" is not a subtask.
- **Checkable:** every subtask has its own check (a test, `check_page`, a command,
  a grep, reading the result back). No check = not done.
- **Few side effects:** a subtask changes only what its output says. It does not
  fix unrelated things on the way; note them for later instead.
- **Order by dependency:** do the leaves first (things nothing else needs to be
  done before), then what builds on them.
- **Independent subtasks can run in parallel:** for 2 or more that do not touch
  the same files, use `spawn` (one agent each) when the work is big enough to be
  worth it. See `references/catalog.md` → subagent-split-check.

Write the plan into `todo` (one item per subtask). Keep it current: mark each done
as it is done.

## 3. Do one subtask at a time

For each subtask, in order:
1. Read exactly the input it names (use `read` with `outline: true` for big files,
   then read only the part you need).
2. Make the smallest change that produces its output (`edit` for changes, `write`
   for new files).
3. Run its check. If the check fails, fix and check again. After two failed
   fixes, step back: re-read the input, the plan may be wrong; split or redo the
   subtask instead of piling on patches.
4. Mark it done in `todo`, with one line of what changed.

Call tools; never write "I'll do X" without doing it in the same step.

## 4. Compose and check the whole

When all subtasks are done, check the **done-when** test from step 1 on the whole
result, not just the pieces: run the project's tests or build, open pages with
`check_page` at phone and desktop width, read the final files once more. Then run
an independent look at the change (`references/catalog.md` → independent-review)
when the change is large or risky.

## 5. Report

Short and true: what was done (file paths), how it was checked, what was not
checked, and anything left open. No claims you did not check.

## Ownership and honesty rules (always)

- **Copying a website:** only a site the user owns or has written permission to
  copy. Follow `references/copy-own-site.md` exactly: it starts with an ownership
  check. A site the user does not own gets an **original** design that may follow
  its general layout ideas, never its text, images, logos, code or brand.
- **Reverse engineering (REA):** only software the user owns or is allowed to
  analyse; never to bypass licensing, DRM or access controls.
- Never put secrets (API keys, passwords) into files, commits or logs.
- Never claim a check passed that you did not run.
