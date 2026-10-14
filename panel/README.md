# Manarouter web panel

The operator panel served by the conductor (control plane) from this directory — plain HTML,
CSS and ES modules. No build step, no framework, no CDN, no external fonts: the files are
served from disk or embedded at compile time and must work on a fully offline install.

All requests are **relative** (`/api/…`); the panel talks only to the origin that served it.

## Files

| File | Role |
|---|---|
| `index.html` | Static shell: sidebar nav, topbar, restart-banner slot, `<main>`, toast region, dialog/drawer roots. Everything else is built dynamically. |
| `app.css` | The whole design system. Greys carry structure; pink (`--pink`) marks interaction/emphasis only; green/amber/red are strictly semantic. |
| `app.js` | Entry point: session gate, hash router (`#/view/param`), health poller wiring, the persistent restart-pending banner, shared `ctx` passed to every view. |
| `api.js` | Single fetch wrapper. Parses the `{ error: { code, message, details } }` envelope, centralises the 401 → login transition, offers `getRaw`/`putRaw` for the config file. |
| `dom.js` | `h()` element builder — the only sanctioned DOM construction. **No `innerHTML` anywhere in the panel**: server strings (provider labels, model ids, error messages, doc text) enter the DOM as text nodes or `textContent` only. Also: validation-problem → field mapping, error blocks, chips, form fields. |
| `format.js` | Pure, DOM-free helpers (uptime/age, chip-class selection, problem grouping, port diffing, combo step moves, hedge-enum parsing). See “Testing”. |
| `poll.js` | Health poll scheduler. Chained `setTimeout` (never overlapping), **stops entirely while `document.visibilityState !== 'visible'`** — a background tab must not hammer a loopback socket — and fires one immediate tick on return. |
| `toast.js` | Global notification area. Success/error/warning/info map onto the semantic colours; error toasts render the backend's full `details` list. |
| `dialog.js` | Promise-based confirm dialog (focus trap, Escape, focus restore) and the one-time client-key reveal (no accidental backdrop dismissal; copy button). |
| `view-login.js` | Password screen when `password_required && !authenticated`. |
| `view-overview.js` | Health stats, snapshot digest/age, counts, live-updated uptime, and the relay reachability probe (`fetch(url, {mode:'no-cors'})` — opaque but proves the socket answers). |
| `view-providers.js` | Dense provider table + detail drawer: accounts (add/toggle/delete), models (add manual/hide/delete), timeouts, headers, route overrides, per-provider **Sync models**. |
| `view-combos.js` | Combo list + editor. Step list is drag-reorderable (plus ↑/↓ buttons for keyboard) and **never sorted** — the array order *is* the priority order. Hedge help text summarises `docs/07-hedged-race.md`. |
| `view-keys.js` | Client keys, allow-lists (one per line), create flow with the show-once plaintext dialog. |
| `view-aliases.js` | Alias table editor (add + delete; the API has no alias PUT). |
| `view-models.js` | All models across providers, filtered by provider/source/text/hidden, with hide/show toggles. |
| `view-quota.js` | Declared windows from the config (real data) + an attempted `GET /api/quota` that degrades into an **honest empty state** if the endpoint does not exist yet. |
| `view-settings.js` | Port editor with apply-on-restart warning, panel password, raw config editor with Validate/Save. |
| `view-docs.js` | Documentation browser; renders doc text verbatim (`textContent`) — markdown shown as source, zero XSS surface. |

## API contract as understood by this panel

Shapes come from `crates/mr-core/src/{model.rs,config.rs}` and the endpoint list in the task
spec. The panel is defensive about list envelopes: `extractList(data, ...keys)` accepts a bare
array or `{ providers: [...] }` / `{ keys: [...] }` / `{ combos: [...] }` / `{ aliases: [...] }`
/ `{ docs: [...] }`.

Contracts verified against the conductor implementation (still verify interactively in a browser):

1. **Account creation** — `POST /api/providers/{id}/accounts` is sent `{ label, identity, secret }`
   (the `Account` struct's fields; the secret is expected to become a server-side `secret_ref`).
2. **Model hide/show** — `POST /api/providers/{id}/models` is treated as an upsert and sent the
   full model object with `hidden` flipped (`hidden` is authored metadata; manual entries win
   over syncs per `docs/03-configuration.md`). If the backend grows a dedicated toggle route,
   change `toggleModelHidden` (providers view) and `toggleHidden` (models view).
3. **Key rotation** — `POST /api/keys/{id}/rotate` replaces a client key's secret in place, preserves its id and policy, invalidates the old secret after commit, and reveals the new plaintext once. The panel asks for confirmation and uses the one-time copy dialog. This rotates Manarouter client credentials, not upstream provider keys.
4. **Quota usage** — `GET /api/quota` returns persisted per-account usage aggregates (all recorded
   time; it is not a live billing-window quota signal). A failure renders an honest unavailable state.
5. **Hedge enum** — `hedge.on_winner_fail` is parsed/built as the externally tagged serde form
   `{"rerun-excluding":{"max_rounds":2}}` / `{"stable-fallback":{"step":1}}` /
   `{"pick-highest-passed":null}`; a bare string form is tolerated on read.
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

### Pure functions (`node`, no browser needed)

`format.js` touches no DOM, `poll.js` takes injected `doc`/`win`, `dom.js`'s pure helpers
(`problemParts`, `fieldMatches`) are exported separately. With `node:test`:

```js
// panel/test/format.test.mjs (suggested location for a future harness)
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  formatUptime, portsDirty, describePortChange, groupProblems, fieldMatches,
  moveStep, remapIndex, remapParticipants, parseWinnerFail, buildWinnerFail,
  relayProbeUrl, deriveProviderHealth, accountState, healthChipClass,
} from '../format.js';

test('formatUptime picks the two most significant units', () => {
  assert.equal(formatUptime(14520), '4 h 2 m');
  assert.equal(formatUptime(59), '59 s');
});

test('portsDirty/describePortChange detect a not-yet-live port change', () => {
  const ports = { relay: { host: '127.0.0.1', port: 8080, running_port: 20900 },
                  control: { host: '127.0.0.1', port: 20901, running_port: 20901 } };
  assert.equal(portsDirty(ports), true);
  assert.deepEqual(describePortChange(ports),
    ['relay will listen on 127.0.0.1:8080 instead of 127.0.0.1:20900']);
});

test('moveStep never sorts; remapIndex keeps positional refs pointing at the same row', () => {
  const steps = ['a', 'b', 'c'];
  assert.deepEqual(moveStep(steps, 0, 2), ['b', 'c', 'a']);
  assert.equal(remapIndex(0, 0, 2), 2);      // moved element
  assert.equal(remapIndex(1, 0, 2), 0);      // shifted left
  assert.deepEqual(remapParticipants([0, 2], 0, 2), [1, 2]);
});

test('hedge on_winner_fail round-trips through the tagged wire form', () => {
  assert.deepEqual(parseWinnerFail({ 'stable-fallback': { step: 2 } }),
    { mode: 'stable-fallback', maxRounds: null, step: 2 });
  assert.deepEqual(buildWinnerFail('rerun-excluding', { maxRounds: 3 }),
    { 'rerun-excluding': { max_rounds: 3 } });
});

test('an all-accounts-disabled provider is intentionally neutral, never red', () => {
  const p = { enabled: true, accounts: [{ enabled: false }, { enabled: false }] };
  assert.equal(deriveProviderHealth(p).cls, 'chip-neutral');
  assert.equal(deriveProviderHealth({ enabled: false }).cls, 'chip-neutral');
});

test('fieldMatches maps deep problem paths onto leaf-named inputs', () => {
  assert.equal(fieldMatches('window_ms', 'hedge.window_ms'), true);
  assert.equal(fieldMatches('window_ms', 'steps[0].model'), false);
});
```

The poller can be exercised with a fake document/window (`createPoller({ tick, doc: fakeDoc,
win: fakeWin })`) asserting that no `tick` runs while `visibilityState === 'hidden'` and one
runs immediately on returning to `visible`.

**Status: this suite was actually executed against `format.js`/`dom.js` in the building
environment (Node 24, `/tmp`, kept out of `panel/` to respect the deliverable file list) and
all assertions passed** — covering uptime/age formatting, chip-class mapping, the
disabled-provider-is-neutral rule, problem grouping and leaf-field matching, port diffing +
probe-URL normalisation, step moves and participant remapping, the hedge-enum round-trip, and
allow-list line handling. To keep it, save the snippet above as `panel/test/format.test.mjs`
and run `node --test panel/test/`.

### Syntax check (performed)

Every module passes `node --check` (Node 24 detects the ES-module syntax; see report).

### What still needs a human in a browser

The panel was **not** executed in a browser in this environment. Manual checklist:

1. Overview: stats fill from `/api/health`; stop the backend → pill turns red, boot screen
   offers Retry; hide the tab for a minute and confirm (via network inspector) no polls fire.
2. Providers: open drawer, add/toggle/delete an account, add a manual model, hide/show,
   Sync models (button must stay disabled while running).
3. Combos: drag a step and verify participants + stable-step references move with it; save;
   reload and confirm order survived exactly.
4. Keys: create → one-time dialog shows `mr-…`; Copy works; deleting asks first.
5. Settings: change a port → amber chip + persistent banner; restart → pill shows
   “restarting…”, banner clears afterwards.
6. 401 path: log out in another tab / expire the session → panel returns to login.
7. 1280×720: no page-level horizontal scrolling on any view; tables scroll internally.

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
* Panel JS+CSS ≈ 60 KB unminified, well under the 100 KB budget.
