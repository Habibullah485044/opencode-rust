# Copying a website the user owns

A copy brings over the site's text, images, styles and code. That is fine for the
user's own site (moving hosts, rebuilding, recovering lost source) and wrong for
anyone else's: their content and design belong to them. So this always starts
with ownership.

## Step 0: ownership (required, before fetching anything)

1. Ask with `ask_user`: "Is <domain> your own site, or do you have written
   permission from its owner to copy it?" Options: "It's mine", "I have
   permission", "No".
2. "No" → do not copy. Offer instead: an original site for the user's own brand
   that may follow the general idea of the layout (section order, kind of
   sections), with new text, new images, new colours and your own code. Say this
   in one line and continue only on that basis.
3. "It's mine" / "I have permission" → when the user can show it cheaply, verify:
   - the project folder already contains the site's source or a git remote for
     the same domain, or
   - the user puts a file you name (e.g. `/habi-verify-<random>.txt` with a code
     you give) on the site, and `webfetch` shows it.
   If they cannot, accept their word but write in the report: "Copied <domain> on
   the user's statement that they own it / have permission."
4. Never copy login-protected pages, other users' data, payment pages or admin
   areas, whoever owns the site.

## Step 1: capture (subtasks, each with a check)

```
fetch_pages     in: start URL                 out: saved HTML of each page in scope   check: every page in the sitemap/nav saved
list_assets     in: saved HTML                out: list of CSS, JS, images, fonts     check: every src/href resolved to an absolute URL
download_assets in: asset list                out: files under assets/ with same names check: count matches; no 404s
rewrite_links   in: pages, assets             out: relative paths to local copies     check: grep finds no links to the old host except intended outbound links
clean           in: pages                     out: analytics, trackers, third-party keys and chat widgets removed  check: grep for gtag/fbq/hotjar/API keys finds nothing
```

How:
- Static pages: `bash` with `curl -L --fail -o` (or `webfetch url` to read).
  Respect the site's rate: a short pause between downloads.
- Pages built by JavaScript (React, Vue…): the HTML is nearly empty. Use the GUI
  browser tools (`browser_navigate`, `browser_snapshot`) to get the rendered
  structure, then rebuild it (step 2) instead of copying the bundle.
- The site's backend (database, forms, logins, payments) cannot be copied from the
  front end. List what needs a backend and ask how the user wants it handled.

## Step 2: rebuild

- Keep the same structure and look; clean up the code: semantic HTML, one
  stylesheet, design tokens on `:root` (see `website.md`).
- Forms: point them where the user says (their backend, a form service) or leave
  them working locally with a clear TODO.

## Step 3: compare (the check that the copy is right)

- `check_page` on every page at 375px and 1440px: must pass.
- Visual compare: open the original and the copy with the browser tools (or take
  screenshots) at the same widths and list differences; fix the ones that matter.
- Click-through: every nav link opens the right local page; no request goes to
  the old host unless intended.

## Report

Pages copied, assets downloaded (count), what was removed (trackers), what could
not be copied (backend features) and how the ownership was established.
