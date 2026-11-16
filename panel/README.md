# Manarouter web panel

The operator panel served by the conductor (control plane) from this directory — plain HTML,
CSS and ES modules. No build step, no framework, no CDN, no external fonts: the files are
served straight from disk (the `panel/` directory is located at startup — nothing is embedded) and must work on a fully offline install.

All requests are **relative** (`/api/…`); the panel talks only to the origin that served it.

## Files

| File | Role |
|---|---|
| `index.html` | Static shell: sidebar nav, topbar, restart-banner slot, `<main>`, toast region, dialog/drawer roots. Everything else is built dynamically. |
| `app.css` | The whole design system. Greys carry structure; pink (`--pink`) marks interaction/emphasis only; green/amber/red are strictly semantic. |
| `app.js` | Entry point: session gate, hash router (`#/view/param`), health poller wiring, the persistent restart-pending banner, shared `ctx` passed to every view. |
| `api.js` | Single fetch wrapper. Parses the `{ error: { code, message, details } }` envelope, centralises the 401 → login transition, offers `getRaw`/`putRaw` for the config file. |
| `dom.js` | `h()` element builder — the only sanctioned DOM construction. **Exactly one deliberate `innerHTML`** — docs markdown in `view-docs.js`, escaped at the door by `markdown.js` — and server strings (provider labels, model ids, error messages, doc text) enter the DOM as text nodes or `textContent` only. Also: validation-problem → field mapping, error blocks, chips, form fields. |
| `format.js` | Pure, DOM-free helpers (uptime/age, chip-class selection, problem grouping, port diffing, combo step moves, hedge-enum parsing, quota-probe candidate validation and provenance). See “Testing”. The hedge-enum helpers (`parseWinnerFail` / `buildWinnerFail`) build the backend's internally tagged form; legacy externally tagged documents are still read. |
| `poll.js` | Health poll scheduler. Chained `setTimeout` (never overlapping), **stops entirely while `document.visibilityState !== 'visible'`** — a background tab must not hammer a loopback socket — and fires one immediate tick on return. |
| `combo-live.js` | The sidebar's live combo visualizer, under the Combos nav entry. Polls `GET /api/activity` (the control plane proxying the relay's `/control/activity`) every 1.5 s through `poll.js`. The block shows the combo and the route it is serving on two lines; when idle it shows the last decision, with its relative age in the tooltip and accessible label rather than a third line. `comboActivityView` is the pure decision — live, recent history, never run, control plane unreachable — so all four states are tested without a DOM. |
| `toast.js` | Global notification area. Success/error/warning/info map onto the semantic colours; error toasts render the backend's full `details` list. |
| `dialog.js` | Promise-based confirm dialog (focus trap, Escape, focus restore) and the one-time client-key reveal (no accidental backdrop dismissal; copy button). |
| `view-login.js` | Password screen when `password_required && !authenticated`. |
| `view-overview.js` | Health stats, snapshot digest/age, counts, live-updated uptime, and the relay reachability probe (`fetch(url, {mode:'no-cors'})` — opaque but proves the socket answers). |
| `view-providers.js` | Dense provider table with pinning, four sort modes and drag reordering, plus an inline detail panel under the selected row: accounts (add/toggle/delete), models (add manual/hide/delete), timeouts, headers, route overrides, per-provider **Sync models**. Creating a provider opens its first-account form immediately, so the new provider is never left without a credential. |
| `view-combos.js` | Combo list + editor. Step list is drag-reorderable and **never sorted** — the array order *is* the priority order. Provider/model pickers are searchable, a step may point at another combo (nested routes expand in place; cycles are refused), and unsaved edits are guarded on close, replacement and navigation. Hedge help text summarises `docs/07-hedged-race.md`. |
| `view-keys.js` | Client keys, allow-lists (one per line), create flow with the show-once plaintext dialog. |
| `view-aliases.js` | Alias table editor (add + delete; the API has no alias PUT). |
| `view-models.js` | All models across providers in one dense table. Label, context, pricing, reasoning and input columns appear only when the loaded rows report them; when unfiltered, rows that report more are listed first. Model ids stay primary and truncate rather than wrap. Search, provider/source/hidden filters, 50/100/250/500 paging, a keyboard-reachable detail drawer (metadata overrides, test, hide, delete), and confirmed bulk resync/delete. |
| `view-quota.js` | Cached upstream quota windows, grouped by provider and account: chips for every quantified window, one card per account coloured by its most exhausted window, and display-only drag reordering for both levels. Refresh is explicit (or opt-in on load); the request ledger from `GET /api/usage` follows underneath. A card read through a candidate walk names the candidate that answered, in its footer. |
| `view-settings.js` | Settings cards: listener ports with the apply-on-restart warning, the relay connection pool, client-key access, upstream proxies, quota display, **quota probing** (the candidate endpoints a generic provider is asked for an allowance — reorderable, with the one-token `POST` and its model), pricing data, the panel password, and the raw configuration with Validate/Save. |
| `view-docs.js` | Documentation browser; renders doc text as markdown into `innerHTML`, through `markdown.js`, which escapes raw HTML (`&`/`<`/`>`/`"`/`'`) first, so a doc cannot inject DOM. |

## API contract as understood by this panel

Shapes come from `crates/mr-core/src/{model.rs,config.rs}` and the endpoint list in the task
spec. The panel is defensive about list envelopes: `extractList(data, ...keys)` accepts a bare
array or `{ providers: [...] }` / `{ keys: [...] }` / `{ combos: [...] }` / `{ aliases: [...] }`
/ `{ docs: [...] }`.

Contracts verified against the conductor implementation (still verify interactively in a browser):

1. **Account creation** — `POST /api/providers/{id}/accounts` is sent `{ label, identity, secret }`
   (the `Account` struct's fields; the secret is expected to become a server-side `secret_ref`).
   `secret` is **required by the panel** and refused before the request is sent; the only way to
   omit it is to tick "This provider needs no API key" for a local server (Ollama, llama.cpp) that
   genuinely accepts credential-less requests. The OAuth path sends `secret: null`, because the
   credential comes from the sign-in flow. Panel flow for a new provider: **Add provider** → its
   first-account form opens immediately → **Add account** (or **Connect** for OAuth) → the inline
   detail panel opens under the provider row with the account listed.
2. **Model hide/show** — `POST /api/providers/{id}/models` is treated as an upsert and sent the
   full model object with `hidden` flipped (`hidden` is authored metadata; manual entries win
   over syncs per `docs/03-configuration.md`). If the backend grows a dedicated toggle route,
   change `toggleModelHidden` (providers view) and `toggleHidden` (models view).
3. **Key rotation** — `POST /api/keys/{id}/rotate` replaces a client key's secret in place, preserves its id and policy, invalidates the old secret after commit, and reveals the new plaintext once. The panel asks for confirmation and uses the one-time copy dialog. This rotates Manarouter client credentials, not upstream provider keys.
4. **Quota usage** — `GET /api/quota` returns persisted per-account snapshots (`provider`,
   `account`, `windows`, `fetched_at`, optional `error`). Only a window with a reported fraction is
   drawn; an account with no quantified window is omitted. A failed refresh keeps the last-good
   windows and shows the error. `POST /api/providers/{id}/accounts/{accountId}/quota/refresh`
   refreshes one account and `POST /api/quota/refresh` refreshes all; the page does not poll
   upstream quota. The recent request ledger is `GET /api/usage?limit=20`.
5. **Hedge enum** — `hedge.on_winner_fail` is sent in the internally tagged form the backend uses
   (`#[serde(tag = "mode")]` in `crates/mr-core/src/dialect.rs`):
   `{"mode":"rerun-excluding","max_rounds":2}` / `{"mode":"stable-fallback","step":2}` /
   `{"mode":"pick-highest-passed"}`. `format.js` (`parseWinnerFail` / `buildWinnerFail`) builds that
   form and still *reads* the older externally tagged shape, so a config written by an earlier
   build keeps loading. The backend form is pinned by
   `on_winner_fail_uses_the_internally_tagged_wire_form` in `dialect.rs`.
6. **Auth kinds** — provider auth is sent as the internally tagged form `{"type":"bearer"}`,
   `{"type":"api-key-header","header":…}`, `{"type":"header","name":…,"prefix":…}`,
   `{"type":"query-param","param":…}`, `{"type":"oauth","platform":…}`.

Error rendering: `error.message` plus **every** entry of `error.details` — always listed in
full (`errorBlock` / toast), and additionally pinned next to their inputs where a field matches
(`applyFieldErrors`, leaf-name matching: `hedge.window_ms` matches `data-field="window_ms"`).
Matched problems are *also* kept in the list — completeness beats cleverness.

## Design-language decisions that are easy to get wrong

* **A provider with every account disabled is neutral grey**, not red, not amber — an
  intentional state (`deriveProviderHealth`). Same for `provider disabled` and `no accounts`.
* **Pending restart is amber** — a warning that a saved port change is not live, not an error.
  The banner has no dismiss button on purpose; it disappears when the restart happens.
* **The restart confirm button is pink, not red** — disruptive but not destructive.
* **Red is reserved for** genuinely dead states and destructive actions (delete provider/key/
  alias/model/step, “Delete provider” warns that stored credentials go with it), and for the
  one big “Relay not reachable” surface on the Overview.
* Model **source chips (synced/manual/inferred) are neutral** — provenance, not health.
* Only structural/semantic greys everywhere else; pink appears on: active nav item, focus
  rings, primary buttons, selected rows, links, the priority-order callout.

## Testing

### Browser end-to-end smoke test

`test/browser.e2e.mjs` exercises the guarded login and client-key create/rotate dialogs in a headless browser against a local mock API. It never contacts the configured conductor or reads user configuration. Run it where Playwright and a matching browser are installed:

```bash
npm install --no-save playwright
npx playwright install chromium
node panel/test/browser.e2e.mjs
```

For Firefox, use `npx playwright install firefox` then `BROWSER=firefox node panel/test/browser.e2e.mjs`. Playwright is intentionally not a project dependency. Set `PLAYWRIGHT_MODULE` to an installed module path only if it is not resolvable from the checkout; set `CHROMIUM_EXECUTABLE` or `FIREFOX_EXECUTABLE` only if using a browser binary not matched to the installed Playwright version. The test exits with code 2 and setup guidance if Playwright is unavailable. The checkout panel files are served by this test’s own local ephemeral HTTP server.

### Pure functions and view logic (`node`, no browser needed)

The suite in `panel/test/*.test.mjs` covers the DOM-free helpers and the renderers that can run
without a browser: API request envelopes, uptime/age formatting, provider presets, probe targets,
reasoning levels, searchable selects, combo live-state selection, quota display ordering,
quota-probe candidate validation and provenance, model grouping and paging, markdown escaping,
OAuth redirects and provider refresh decisions. The account editors are covered the same way:
account tags, the Settings people card, and a combo step's account selection each mount the real
view against a stubbed DOM and `fetch`, so the tests assert the request body rather than the
intent. Run it with:

```bash
node --test panel/test/*.test.mjs
```

`browser.e2e.mjs` is separate: it drives the guarded login and client-key dialogs against a local
mock API in Playwright, without touching the configured conductor. Its setup is above.

### Current verification

At the time of writing, `node --test panel/test/*.test.mjs` runs 403 tests with no failures, and
the Rust workspace passes `cargo fmt --all -- --check`, `cargo check --workspace --all-targets`
and `cargo test --workspace`.

The panel was hard-reloaded route by route in headless Chromium at 1280×900 and 390×844. Every
route — overview, providers, combos, a combo detail, models, keys, aliases, quota and settings —
finished loading with no uncaught exception, error card, dialog, drawer or page-level horizontal
overflow. At 390 px the models table scrolls inside its own pane (816 px of table in a 358 px
column) instead of widening the document.

### Manual checks for a live install

These need a real backend and, in some cases, deliberate failures:

1. Overview: stats fill from `/api/health`; stop the backend → pill turns red, boot screen
   offers Retry; hide the tab for a minute and confirm (via network inspector) no polls fire.
2. Providers: **Add provider** → its first-account form opens on its own; submitting with an empty
   secret is refused inline, ticking "needs no API key" allows it; after adding, the inline detail
   panel opens showing the account. Then toggle/delete an account, add a manual model, hide/show,
   and Sync models (button must stay disabled while running).
3. Combos: drag a step and verify positional references move with it; add a nested combo step
   and confirm it expands in place; try to create a cycle and confirm the error names the chain.
   Edit a combo, then close the drawer, start a new combo, and navigate away: each path asks first,
   and keeping the draft must leave exactly one editor with no stale prompt afterwards.
4. Keys: create → one-time dialog shows `mr-…`; Copy works; deleting asks first.
5. Settings: change a port → amber chip + persistent banner; restart → pill shows
   “restarting…”, banner clears afterwards.
6. 401 path: log out in another tab / expire the session → panel returns to login.
7. 1280×900 and 390×844: no page-level horizontal scrolling on any view; tables scroll
   internally.

## Constraints honoured

* No build step, bundler, CDN, external fonts or icon fonts (icons are inline SVG built via
  `createElementNS`). The only fetches are same-origin `/api/…` calls plus the Overview's
  loopback relay probe, which is a `mode:'no-cors'` reachability check that sends no data
  anywhere.
* System font stack; monospace stack (`--mono`) for keys, ids, digests, model ids.
* Keyboard: skip link, visible pink `:focus-visible` rings, focus-trapped dialogs with focus
  restore, `aria-label`s on every icon-only button, `aria-current` nav/selection, tables use
  `scope="col"`, rows openable via keyboard (Open button / Enter).
* Live region for toasts (`aria-live="polite"`, errors `role="alert"`).
