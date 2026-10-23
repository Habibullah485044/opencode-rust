# Benchmark: professional homepage (frontend only)

Paste the six lines below, unchanged, into each tool. Start each run in its own
new, empty folder, with the same model.

```
In this empty folder, build a professional homepage for "Lumen Coffee Roasters" (a fictional specialty coffee brand) as plain index.html, styles.css and script.js — no frameworks, no build step, no network requests except Google Fonts, and no image files or image URLs (use inline SVG or CSS).
In order: a <header> with the brand name and a <nav> linking to every section, then <section>s with ids hero (one <h1> and a call-to-action button), features, menu (at least 4 coffees with prices), testimonials, contact, then a <footer>.
Below 768px wide the nav collapses behind a menu button with aria-controls and aria-expanded that opens and closes it; it must look polished from 360px to 1440px with no horizontal scrolling.
Colors live in CSS custom properties on :root, dark mode follows prefers-color-scheme, every svg or img has an accessible name, and the html has lang, a viewport meta, a title and a meta description.
The contact form (name, email, message, each with a <label>) uses novalidate and script.js: invalid fields get aria-invalid="true" and a visible message, nothing is sent, and a valid submit shows a thank-you text in an element with id form-status.
Work in parallel: one agent writes the HTML and CSS, one writes script.js, one reviews both in a browser-like way; end with a short summary of what you built.
```

## Scoring

After each run:

```bash
python -I bench/homepage/score.py <the folder the tool worked in>
```

It checks the page's structure and code (60 points), then opens it in headless
Edge or Chrome at 360, 768 and 1440 px wide: script errors, horizontal
scrolling, the mobile menu and the contact form (40 points). It saves
screenshots at those three widths in `bench/homepage/results/<folder name>/`
so you can judge the design yourself; the score does not rate looks. Also note for each run the
time it took and the tokens or credits it used (`/usage` in opencode-rust).
