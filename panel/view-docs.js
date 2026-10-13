// view-docs.js — documentation browser (GET /api/docs → list, GET /api/docs/{name} → text).
//
// Docs are rendered as VERBATIM plain text through textContent. They are markdown source; a
// markdown-to-HTML renderer with no sanitizer would be an XSS surface fed partly by third-party
// content, so this panel deliberately shows the source instead. Operators read markdown fine.

import { api } from './api.js';
import { h, emptyState, errorBlock } from './dom.js';
import { extractList } from './format.js';

export async function renderDocs(mount, ctx, selected) {
  const listEl = h('ul', { class: 'docs-nav' });
  const content = h('div', { class: 'card' });
  mount.replaceChildren(h('div', { class: 'docs-layout' },
    h('div', { class: 'card' }, h('h2', {}, 'Documents'), listEl),
    content));

  let docs = [];
  try {
    docs = extractList(await api.get('/api/docs'), 'docs');
  } catch (err) {
    content.replaceChildren(errorBlock(err));
    return;
  }

  if (!docs.length) {
    listEl.append(h('li', { class: 'hint' }, 'none'));
    content.replaceChildren(emptyState('No documentation is available in this build.',
      'GET /api/docs returned an empty list.'));
    return;
  }

  renderList();

  // Selection: URL param, else the first document — the pane is never silently empty.
  const wanted = selected && docs.find((d) => docName(d) === selected) ? selected : docName(docs[0]);
  if (wanted !== selected) history.replaceState(null, '', `#/docs/${encodeURIComponent(wanted)}`);
  await load(wanted);

  function docName(d) {
    return d.name ?? d.path ?? d.title ?? '';
  }

  function renderList() {
    listEl.replaceChildren();
    for (const d of docs) {
      const btn = h('button', { type: 'button' },
        h('div', { class: 'dtitle' }, d.title ?? docName(d)),
        d.purpose ? h('div', { class: 'dpurpose' }, d.purpose) : null);
      btn.addEventListener('click', () => {
        history.replaceState(null, '', `#/docs/${encodeURIComponent(docName(d))}`);
        [...listEl.querySelectorAll('button')].forEach((b) => b.removeAttribute('aria-current'));
        btn.setAttribute('aria-current', 'true');
        load(docName(d));
      });
      if (docName(d) === wanted) btn.setAttribute('aria-current', 'true');
      listEl.append(h('li', {}, btn));
    }
  }

  async function load(name) {
    content.replaceChildren(h('p', { class: 'hint' }, 'loading…'));
    try {
      const text = await api.getRaw(`/api/docs/${encodeURIComponent(name)}`);
      content.replaceChildren(
        h('h2', { style: 'text-transform:none;letter-spacing:0;' }, docs.find((d) => docName(d) === name)?.title ?? name),
        h('pre', { class: 'doc-text' }, text),
      );
    } catch (err) {
      content.replaceChildren(errorBlock(err));
    }
  }
}
