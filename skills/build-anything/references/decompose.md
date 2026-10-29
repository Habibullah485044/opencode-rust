# Decomposing a goal into small functions

The goal is a function `goal(input) -> result`. Write it as a composition of
smaller functions until each one is small enough to do and check in a few tool
calls. Then run them leaf-first and compose.

## The subtask card

```
name:     short_verb_phrase           # what it does, in snake_case
input:    files / data it reads       # exact paths or the output of another subtask
output:   what exists or is true after
check:    how you prove the output    # a command, check_page, a test, a grep, a read-back
depends:  other subtask names         # empty for leaves
```

A good card answers: *could a stranger do this one piece from the card alone and
know when it is done?* If not, it is too vague or too big.

## How small is small

Split again when a subtask:
- touches more than ~5 files or would add more than ~150 lines,
- has an output with "and" in it that are really two outputs,
- has no single check that proves it,
- would take more than ~8 tool calls.

Do not split below the level of one meaningful check: "add one CSS rule" is too
small to be its own subtask unless it is the whole job.

## Order

1. Draw the dependency list: for each card, which cards must be done first.
2. Leaves first: cards with nothing in `depends`.
3. Then each card whose dependencies are all done.
4. Cards with no dependency on each other and no shared files are independent:
   they can go to parallel subagents (`spawn`) when each is big enough to be worth
   a separate agent (see MCode Stack `subagent-split-check`). Small ones: just do
   them one after another.

## Composition = integration check

Pieces that each pass can still fail together. After composing, always run the
whole-goal check (the done-when test). When it fails, find which piece's output
does not match the next piece's input, fix that piece, re-run its own check, then
the whole check again.

## Purity (few side effects)

A subtask changes only what its output says. Found another problem on the way?
Write it down as a new card (or a line in the report). Fixing it inside the
current subtask makes the change harder to check and to undo.

## Worked examples

### Website: "landing page for a coffee shop"
```
define_content        in: request            out: copy for each section (names, prices, hours)    check: every requested section has text
design_tokens         in: request            out: :root colours, type scale, spacing in styles.css check: contrast ≥ 4.5:1 for body text
build_structure       in: content            out: semantic index.html (header, main sections, footer) check: check_page finds every heading
build_header_nav      in: structure          out: header + nav + menu button                       check: check_page at 375px: menu opens/closes
build_sections        in: structure, tokens  out: hero, menu cards, contact styled                 check: check_page at 375 and 1440: no overflow
add_behaviour         in: header_nav         out: script.js (menu toggle, smooth scroll)            check: node --check script.js; menu test
final_check           in: all                out: report                                           check: check_page passes at both widths
```
`build_header_nav` and `build_sections` are independent after `build_structure`.

### Bug: "the cart total is wrong"
```
reproduce      in: report          out: a failing test or exact steps      check: it fails the same way the user saw
locate         in: failing test    out: the function and line at fault     check: a breakpoint/print shows the wrong value there
fix            in: location        out: smallest code change               check: the new test passes
guard          in: fix             out: tests for neighbours (rounding, empty cart, discounts)  check: they pass
regression     in: all             out: full test run                      check: everything that passed before still passes
```

### Feature in an existing app
```
map_area       in: request, repo   out: list of files/functions involved   check: grep finds every caller
design         in: map             out: the interface (names, data shapes)  check: it covers every case in the request
implement_core in: design          out: the logic + unit tests             check: tests pass
wire_ui        in: core            out: UI/route calls the core            check: manual or browser check
docs           in: all             out: README/changelog lines             check: read back
```

### Research or a question
```
frame          in: question        out: 3–5 sub-questions                  check: answering all of them answers the question
gather         in: each sub-question  out: facts with sources (webfetch query/url, files)  check: each fact has a source
reconcile      in: facts           out: where sources agree/disagree       check: no claim without a source
answer         in: reconcile       out: short answer + sources             check: it answers the original question
```

### Writing (docs, a report, a proposal)
```
audience_goal  in: request         out: who reads it, what they must do after   check: one sentence
outline        in: audience_goal   out: headings with one-line points       check: every point serves the goal
draft_sections in: outline         out: text per heading                    check: each section does what its heading says
edit           in: draft           out: shorter, plainer text               check: read aloud test; no claim unsupported
```

## Failure handling

- A check fails twice after fixes: stop patching. Re-read the subtask's input and
  card. Usually the card is wrong (too big, or its input was misunderstood). Split
  it or rewrite it.
- A subtask turns out unnecessary: drop it, say so in `todo`.
- New subtasks appear: add cards; do not silently grow an existing one.
