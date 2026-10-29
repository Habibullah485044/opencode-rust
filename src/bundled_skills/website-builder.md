---
name: website-builder
description: Build or redesign a web page or site
---

# Building a website

Use this for any request to build, redesign or extend a web page or site.

## Before writing

1. Look first: `list` the folder and `read` any existing HTML, CSS, package.json
   or framework config. Extend what is there; do not start a second app.
2. No framework in the project and none asked for: one `index.html`, one
   `styles.css`, one `script.js`. No build step.
3. Settle the content before the code: the sections, their order, and real
   text for each (no "Lorem ipsum", no "Your headline here").

## Sections a complete page has

- Header: name or logo, a few nav links that jump to sections on the page.
- Hero: one clear headline, one sentence under it, one main button.
- The body sections the request names (features, pricing, gallery, menu…),
  each with a heading and real content.
- A contact or call-to-action section, and a footer.

## Quality rules

- Colours and fonts as CSS variables on `:root`; one accent colour.
- Spacing on a scale (4/8/16/24/32/48/64 px). Text at least 16px; line
  length under about 75 characters.
- Mobile first: works at 375px wide with no sideways scrolling; a menu that
  collapses on small screens; images `max-width: 100%`.
- Every image has `alt`; buttons are `<button>` or `<a>`, not clickable divs;
  colour contrast readable; visible focus outlines.
- Forms: labels for every field, a `required` check, and a visible message
  after submit (no real sending unless asked).
- No external requests the user did not ask for. Icons as inline SVG.

## Finishing

1. Re-read every file you wrote once, start to end: broken tags, missing
   closing braces, links to files that do not exist, ids the script looks up
   that are not in the HTML.
2. Tell the user how to open it (the file path, or the command that serves it)
   and list the sections in one line each. Say what you did not do.
