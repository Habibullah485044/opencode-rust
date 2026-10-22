#!/usr/bin/env bash
# Headless checks for the desktop UI.
#
# Two suites, both run in real Microsoft Edge (no Node, no Playwright):
#   panel.js   — gui.html behaviour: URL bar, nav buttons, geometry, approvals
#   runtime.js — the JS injected into the preview webview (snapshot/click/type/wait)
#
# Usage: tests/gui/run.sh        (set EDGE=/path/to/msedge to override)
set -u

root=$(cd "$(dirname "$0")/../.." && pwd)

edge=${EDGE:-}
if [ -z "$edge" ]; then
  for c in "/c/Program Files (x86)/Microsoft/Edge/Application/msedge.exe" \
           "/c/Program Files/Microsoft/Edge/Application/msedge.exe"; do
    if [ -f "$c" ]; then edge=$c; break; fi
  done
fi
if [ -z "$edge" ]; then
  edge=$(command -v msedge || true)
fi
if [ -z "$edge" ]; then
  echo "SKIP: Microsoft Edge not found (set EDGE=/path/to/msedge)"
  exit 2
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
rc=0

# Chromium wants a Windows path when the page is on another drive.
win() { cd "$(dirname "$1")" && printf '%s/%s' "$(pwd -W)" "$(basename "$1")"; }

run() {
  local name=$1 page=$2 out
  out=$("$edge" --headless=new --disable-gpu --no-sandbox --window-size=1280,800 \
        --dump-dom "file://$(win "$page")" 2>/dev/null \
        | sed -n '/<pre id="testout">/,/<\/pre>/p' | sed 's/<[^>]*>//g')
  if [ -z "$out" ]; then
    echo "FAIL $name: page produced no results"
    rc=1
    return
  fi
  echo "$out"
  if printf '%s\n' "$out" | grep -q '^FAIL'; then
    echo "FAIL $name"
    rc=1
  else
    echo "PASS $name ($(printf '%s\n' "$out" | grep -c '^PASS') assertions)"
  fi
}

# --- suite 1: the app page -------------------------------------------------
{
  head -n -2 "$root/src/gui.html"     # drop </body></html>
  printf '<script>\n'
  cat "$root/tests/gui/panel.js"
  printf '\n</script>\n</body>\n</html>\n'
} > "$tmp/panel.html"

# --- suite 2: the injected page runtime ------------------------------------
# Extracted from the Rust source so the test can never drift from the shipped
# script (a copy would silently pass against an old version).
sed -n '/pub const RUNTIME_JS/,/^})();"#;$/p' "$root/src/browser.rs" \
  | sed '1s/.*r#"//' \
  | sed '$s/"#;$//' > "$tmp/runtime.js"

{
  cat <<'HTML'
<!doctype html>
<html><head><meta charset="utf-8"><title>Snapshot Fixture</title></head>
<body>
<h1>Fixture Title</h1>
<form id="f"><input id="i1" placeholder="Email"><button id="b1" type="button">Save</button></form>
<a href="https://example.com/x">Docs</a>
<div style="display:none"><button id="hidden">Nope</button></div>
<script>
HTML
  cat "$tmp/runtime.js"
  printf '</script>\n<script>\n'
  cat "$root/tests/gui/runtime.js"
  printf '</script>\n</body></html>\n'
} > "$tmp/runtime.html"

run "gui.html panel" "$tmp/panel.html"
run "browser runtime" "$tmp/runtime.html"
exit $rc
