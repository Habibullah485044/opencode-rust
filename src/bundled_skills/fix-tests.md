---
name: fix-tests
description: Make failing tests pass, at most 3 rounds
---

# Fixing failing tests

Use this when the user asks to fix the tests, make CI pass, or when a check
after your work reports failing tests.

## Find the command

Pick the project's own test command, in this order: what the user named; a
`test` script in package.json; `cargo test` for Cargo.toml; `pytest` when
there is a tests folder or pytest config; `go test ./...` for go.mod. Run only
the failing package or file first when you know it; it is faster.

## The loop (at most 3 rounds)

1. Run the tests with `bash`. Read the FIRST failure in full: the test name,
   the assertion, the expected and actual values, the file and line.
2. Read the test and the code it calls before changing anything.
3. Decide which is wrong. Fix the code when the test states the intended
   behaviour. Change the test only when the behaviour changed on purpose
   (the user asked for it) and say so.
4. Make the smallest change that fixes that failure. Run the tests again.

Never make a test pass by deleting it, skipping it, loosening its assertion
to nothing, or special-casing the test input in the code.

## Stop and report

Stop after 3 rounds, or sooner when the failure needs something you cannot do
(a missing service, a secret, a network call). Report: the command you ran,
what passes now, what still fails with its first error line, and what you
think the remaining cause is.
