---
name: code-research
description: When to use the Context7 docs tools and the ast-grep code-search tools
---

# Code research with MCP tools

Use this skill only when tools whose names start with `mcp__context7__` or
`mcp__ast-grep__` are in your tool list. If they are not there, ignore it.

## Library documentation (Context7)

Use it before writing code against a third-party library when you are not sure
your knowledge is current: the version in Cargo.toml or package.json is newer
than you know, or the compiler says a function or field does not exist.

1. Call `mcp__context7__resolve-library-id` with the library name and pick the
   best match.
2. Call `mcp__context7__query-docs` with that id and a short, specific question.
3. Follow each tool's input schema for the argument names.

Do not use it for this project's own code, or for the Rust or Python standard
library when you already know the answer.

## Structural code search (ast-grep)

Use it when plain `grep` gives too many false hits, for example "every call to
`foo` with two arguments" or "every `unwrap()` inside an async function".

1. If you are not sure how a construct looks to ast-grep, call
   `mcp__ast-grep__dump_syntax_tree` on a small code sample first.
2. Search with `mcp__ast-grep__find_code`. In patterns, `$NAME` matches one
   syntax node and `$$$ARGS` matches any number of them.
3. For conditions such as "inside" or "not inside", write a YAML rule, check it
   with `mcp__ast-grep__test_match_code_rule` on a sample, then run
   `mcp__ast-grep__find_code_by_rule`.

Use the normal `grep` tool for simple text searches; it is faster.
