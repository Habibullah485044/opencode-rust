---
name: git-commit
description: Write a commit message or commit
---

# Writing a commit

Use this when the user asks to commit, or asks for a commit message.

1. `git status --short` and `git diff --staged` (or `git diff` when nothing is
   staged) to see what changed. Read enough of the diff to know WHY it
   changed, not only which files.
2. `git log --oneline -15` to copy the repository's style: tense, length,
   prefixes such as `feat:` or `fix(scope):` only if the log uses them.
3. Write the message:
   - First line under about 72 characters: what the change does for the
     user or the code, not "update files".
   - A blank line, then a short body only when the reason is not obvious
     from the first line: what was wrong, and why this is the fix.
   - Never paste the diff or list every file.
4. Stage only the files that belong to this change. Never stage secrets
   (`.env`, keys, tokens), build output, or files unrelated to the request;
   say which ones you left out.
5. Commit only when the user asked you to commit; otherwise show the message.
   Never push, amend, rebase or force anything unless the user asked for
   exactly that. Never skip hooks with `--no-verify`. If a hook fails, fix the
   cause and make a new commit.
