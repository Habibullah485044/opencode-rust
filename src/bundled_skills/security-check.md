---
name: security-check
description: Check changes for secrets and security bugs
---

# Security check

Use this before a commit or release when asked to check security, or when
the work touched login, payments, file uploads, shell commands or config.

## 1. Secrets (always)

Search the changed files (`git diff` plus untracked files) with `grep` for:
API keys and tokens (`sk-`, `ghp_`, `AKIA`, `xox`, `-----BEGIN`, long random
strings assigned to names like key, token, secret, password), `.env` files,
and private keys. A real secret found: do not print it in full; name the file
and line, tell the user to rotate it, and move it to an environment variable
or a git-ignored file. Check `.gitignore` covers `.env` and key files.

## 2. Common mistakes in the changed code

- Shell commands built from user input (command injection). Use argument
  lists, never string concatenation into a shell.
- SQL built by string formatting (SQL injection). Use parameters.
- HTML built from user input without escaping (XSS), `innerHTML` with
  untrusted text, `eval`, `new Function`.
- Paths from user input without confining them to a folder (`../` escapes).
- Login and access: routes or API handlers with no auth check; passwords
  stored or compared in plain text; secrets in front-end code.
- `CORS *` with credentials, disabled TLS verification, debug mode on.
- New dependencies: a name that looks like a typo of a popular package.

## 3. Report

List findings by severity (high, medium, low) with file:line and a one-line
fix each. Fix the high ones only when the user asked you to fix; otherwise
ask. If nothing is found, say what you checked, so "nothing found" means
something.
