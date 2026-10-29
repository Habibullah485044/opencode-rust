# Building and changing websites

Follow the method in `../SKILL.md`; this file is what a website needs at each
step. The checks below are what makes a site look professional instead of
"almost right".

## Files and structure

- Plain sites: `index.html`, `styles.css`, `script.js` (more pages as `about.html`
  etc.). Frameworks only when the user asks or the project already uses one; then
  follow the project's existing structure and scripts (`npm run dev` as a
  background `bash` job).
- Semantic HTML: `header`, `nav`, `main`, `section` (each with a heading),
  `footer`; one `h1`; buttons are `<button>`, links are `<a href>`.
- Every image has `alt`; decorative ones `alt=""`. Inline SVG for icons (no icon
  fonts, no external icon kits unless asked).
- Prefer no external requests: system or Google fonts at most; images either
  user-provided, generated as CSS/SVG, or downloaded into the project with the
  source noted. Never hotlink.

## Design system first (subtask `design_tokens`)

Put these on `:root` before styling anything:
- 4–6 colours: background, surface, text, muted text, one accent, its text colour.
  Body text contrast ≥ 4.5:1, large text ≥ 3:1.
- A type scale (e.g. 0.875 / 1 / 1.25 / 1.6 / 2.2 / 3 rem), one or two font
  families, line height ~1.5 for body.
- Spacing on an 8px grid, a max content width (1100–1200px), one radius.
- A dark scheme only if asked, via `prefers-color-scheme` or a toggle.

## Layout rules that avoid the usual bugs

- `*{box-sizing:border-box}`, `img,svg{max-width:100%;height:auto}`,
  `nav ul{list-style:none;margin:0;padding:0}`.
- Mobile first; layouts that wrap: `display:grid;grid-template-columns:
  repeat(auto-fit,minmax(min(260px,100%),1fr))` or flex with `flex-wrap`.
- No fixed widths over 320px; no `100vw` (it causes sideways scroll).
- Phone menu: `<button aria-controls="nav" aria-expanded="false">` with a visible
  icon, toggled in `script.js`; nav links stack under 768px.
- Touch targets ≥ 44px; visible focus styles (`:focus-visible`).

## Behaviour (`script.js`)

- Small, no framework: menu toggle, smooth scroll for in-page links (or CSS
  `scroll-behavior:smooth`), form validation with inline messages.
- Never let a missing element crash the script: check `if (el)` before use.
- `node --check script.js` must pass.

## Checks (run them, do not assume)

- `check_page` on each page at 375px and 1440px. It reports sideways scroll and
  where it starts, broken images, failed loads, script errors, a phone menu that
  does not open/close, and expected texts missing. Fix everything it reports;
  re-run until it passes.
- Read the final HTML once: one `h1`, headings in order, every section from the
  request present, the user's names spelled exactly as given.
- When the GUI browser tools exist (`browser_navigate`, `browser_snapshot`,
  `browser_screenshot`), look at the page yourself after `check_page` passes.

## Changing an existing site

1. `list` and `read` (outline first) to learn its structure and existing tokens.
2. Reuse its classes, colours and patterns; do not introduce a second style system.
3. Change only what the request needs; run `check_page` before and after to be
   sure nothing else broke.

## Copying a site

Only with ownership: `copy-own-site.md`.
