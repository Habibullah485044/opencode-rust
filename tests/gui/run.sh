#!/usr/bin/env bash
# Headless checks for the desktop UI.
#
# Two suites, both run in a real headless Chromium browser (Edge, Chrome or
# Chromium; no Node, no Playwright):
#   panel.js   — gui.html behaviour: URL bar, nav buttons, geometry, approvals
#   runtime.js — the JS injected into the preview webview (snapshot/click/type/wait)
#
# Usage: tests/gui/run.sh   (Windows Git Bash, Linux, macOS)
#        EDGE=/path/to/msedge or CHROME=/path/to/chrome picks the browser.
set -u

root=$(cd "$(dirname "$0")/../.." && pwd)

browser=${EDGE:-${CHROME:-}}
if [ -z "$browser" ]; then
  for c in "/c/Program Files (x86)/Microsoft/Edge/Application/msedge.exe" \
           "/c/Program Files/Microsoft/Edge/Application/msedge.exe" \
           "/c/Program Files/Google/Chrome/Application/chrome.exe" \
           "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge" \
           "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
           "/Applications/Chromium.app/Contents/MacOS/Chromium"; do
    if [ -f "$c" ]; then browser=$c; break; fi
  done
fi
if [ -z "$browser" ]; then
  for name in msedge microsoft-edge microsoft-edge-stable google-chrome \
              google-chrome-stable chromium chromium-browser chrome; do
    if command -v "$name" >/dev/null 2>&1; then browser=$(command -v "$name"); break; fi
  done
fi
if [ -z "$browser" ]; then
  echo "SKIP: no Edge/Chrome/Chromium found (set EDGE= or CHROME= to its path)"
  exit 2
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
rc=0

# file:// URL for a local file. Git Bash needs the Windows form (C:/...);
# Linux and macOS paths are already right.
file_url() {
  local dir
  dir=$(cd "$(dirname "$1")" && pwd)
  if command -v cygpath >/dev/null 2>&1; then
    printf 'file:///%s' "$(cygpath -m "$dir/$(basename "$1")")"
  else
    printf 'file://%s/%s' "$dir" "$(basename "$1")"
  fi
}

run() {
  local name=$1 page=$2 out
  out=$("$browser" --headless=new --disable-gpu --no-sandbox --window-size=1280,800 \
        --dump-dom "$(file_url "$page")" 2>/dev/null \
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
  sed '$d' "$root/src/gui.html" | sed '$d'   # drop </body></html> (portable)
  printf '<script nonce="habi-ui">\n'
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
