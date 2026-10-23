// markdown.js — a small, pure, dependency-free Markdown → HTML renderer for the docs viewer.
//
// THE ONE RULE: escape the whole input first, format second.
//
//   renderMarkdown() HTML-escapes every character of its argument (`& < > " '`) before a single
//   formatting rule runs. Everything below therefore operates on text that is *already escaped*:
//   the only `<` left in the working string is the one inside `&lt;`, and the only tags in the
//   output are the ones this file writes itself. Raw HTML in a document can never become markup —
//   it shows up as visible text. That is also the entire reason `content.innerHTML =
//   renderMarkdown(text)` in view-docs.js is safe: it is the one sanctioned innerHTML in the
//   panel, and it is only ever fed this function's output.
//
// PIPELINE (ordered; each stage small, pure and separately testable):
//   1. escapeHtml       & < > " ' → entities, once, at the door
//   2. normaliseLines   CRLF / CR → LF, then split
//   3. parseBlocks      line-based block scanner: fenced code, headings, hr, pipe tables,
//                       blockquotes, lists, paragraphs. Blocks keep their *escaped* text.
//   4. renderBlocks     each block → HTML, recursing into nested block lists (quotes, lists)
//   5. renderInline     one left-to-right scanner for code spans, links, autolinks, emphasis
//   6. sanitizeUrl      scheme allow-list; anything else is rendered as plain text
//   7. slugify/uniqueSlug  deterministic, collision-free heading ids (`-2`, `-3`, …)
//
// DELIBERATELY OUT OF SCOPE (keeping the file small and honest): images, setext headings,
// reference links, footnotes, raw HTML blocks, syntax highlighting, indented (4-space) code
// blocks, link titles, and emphasis nested more than one level. Each one degrades to literal
// escaped text rather than to a guess.

'use strict';

// ---------------------------------------------------------------------------
// 1. Escaping — done once, before anything else looks at the input
// ---------------------------------------------------------------------------

const ENTITY = {
  '&amp;': '&',
  '&lt;': '<',
  '&gt;': '>',
  '&quot;': '"',
  '&#39;': "'",
};

/** Escape every HTML-significant character. `&` first, or the entities double-escape. */
export function escapeHtml(text) {
  return String(text ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

/** One-pass reverse of escapeHtml, for slug text and tests. `&amp;lt;` decodes to `&lt;`. */
export function decodeEntities(text) {
  return String(text ?? '').replace(/&(?:amp|lt|gt|quot|#39);/g, (m) => ENTITY[m]);
}

// ---------------------------------------------------------------------------
// 2. Line normalisation
// ---------------------------------------------------------------------------

function normaliseLines(markdown) {
  return String(markdown ?? '').replace(/\r\n?/g, '\n').split('\n');
}

function isBlank(line) {
  return line === undefined || /^[ \t]*$/.test(line);
}

function indentOf(line) {
  const m = /^ */.exec(line);
  return m ? m[0].length : 0;
}

function nextNonBlank(lines, from) {
  for (let i = from; i < lines.length; i += 1) {
    if (!isBlank(lines[i])) return i;
  }
  return -1;
}

// ---------------------------------------------------------------------------
// 3. Block scanning
// ---------------------------------------------------------------------------

// Hard cap on structural nesting: document text is untrusted and parseBlocks recurses.
const MAX_BLOCK_DEPTH = 16;

const RE = {
  heading: /^ {0,3}(#{1,6})(?:[ \t]+(.*?))?[ \t]*$/,
  fenceOpen: /^ {0,3}(`{3,}|~{3,})[ \t]*([^\s]*)[ \t]*$/,
  fenceClose: /^ {0,3}(`{3,}|~{3,})[ \t]*$/,
  hr: /^ {0,3}(?:(?:\*[ \t]*){3,}|(?:-[ \t]*){3,}|(?:_[ \t]*){3,})$/,
  // The block scanner runs on escaped text, so a blockquote marker reads `&gt;`, never `>`.
  blockquote: /^ {0,3}&gt;[ \t]?(.*)$/,
  listItem: /^( *)([-*+]|\d{1,9}[.)])[ \t]+(.*)$/,
  tableDelim: /^ {0,3}\|?[ \t]*:?-+:?[ \t]*(?:\|[ \t]*:?-+:?[ \t]*)*\|?[ \t]*$/,
};

function matchFenceOpen(line) {
  const m = RE.fenceOpen.exec(line);
  if (!m) return null;
  return {
    marker: m[1][0],
    length: m[1].length,
    lang: (m[2] || '').replace(/[^A-Za-z0-9_+.\-]/g, ''),
  };
}

function isFenceClose(line, open) {
  const m = RE.fenceClose.exec(line);
  return Boolean(m) && m[1][0] === open.marker && m[1].length >= open.length;
}

function matchListItem(line) {
  const m = RE.listItem.exec(line);
  if (!m) return null;
  const ordered = /\d/.test(m[2][0]);
  let text = m[3];
  let checked = null;
  const task = /^\[([ xX])\][ \t]+(.*)$/.exec(text);
  if (task) {
    checked = task[1].toLowerCase() === 'x';
    text = task[2];
  }
  return {
    indent: m[1].length,
    ordered,
    number: ordered ? Number.parseInt(m[2], 10) : null,
    text,
    checked,
  };
}

function isListItemLine(line) {
  return matchListItem(line) !== null;
}

function isTableStart(lines, i) {
  const head = lines[i];
  const delim = lines[i + 1];
  return Boolean(head && delim)
    && head.includes('|')
    && delim.includes('|')
    && RE.tableDelim.test(delim);
}

/** True when `lines[i]` begins a block other than a paragraph. */
function startsBlock(lines, i) {
  const line = lines[i];
  if (line === undefined || isBlank(line)) return true;
  if (matchFenceOpen(line)) return true;
  if (RE.hr.test(line)) return true;
  if (RE.heading.test(line)) return true;
  if (RE.blockquote.test(line)) return true;
  if (isListItemLine(line)) return true;
  if (isTableStart(lines, i)) return true;
  return false;
}

/**
 * Scan escaped lines into block objects. Pure; nested structures (quotes, list items) recurse
 * through here, which is what makes one level of nesting (and more) fall out for free.
 */
function parseBlocks(lines, depth = 0) {
  if (depth > MAX_BLOCK_DEPTH) return [{ type: 'paragraph', lines }];
  const blocks = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];

    if (isBlank(line)) {
      i += 1;
      continue;
    }

    const fence = matchFenceOpen(line);
    if (fence) {
      const body = [];
      i += 1;
      while (i < lines.length && !isFenceClose(lines[i], fence)) {
        body.push(lines[i]);
        i += 1;
      }
      if (i < lines.length) i += 1; // closing fence
      blocks.push({ type: 'code', lang: fence.lang, text: body.join('\n') });
      continue;
    }

    if (RE.hr.test(line)) {
      blocks.push({ type: 'hr' });
      i += 1;
      continue;
    }

    const heading = RE.heading.exec(line);
    if (heading) {
      blocks.push({
        type: 'heading',
        level: heading[1].length,
        text: (heading[2] ?? '').replace(/[ \t]+#+[ \t]*$/, '').trim(),
      });
      i += 1;
      continue;
    }

    if (isTableStart(lines, i)) {
      const header = splitRow(lines[i]);
      const align = splitRow(lines[i + 1]).map(alignOf);
      const rows = [];
      i += 2;
      while (i < lines.length && !isBlank(lines[i]) && lines[i].includes('|')) {
        rows.push(fitRow(splitRow(lines[i]), header.length));
        i += 1;
      }
      blocks.push({ type: 'table', header, align, rows });
      continue;
    }

    if (RE.blockquote.test(line)) {
      const quoted = [];
      while (i < lines.length) {
        const m = RE.blockquote.exec(lines[i]);
        if (m) {
          quoted.push(m[1]);
          i += 1;
          continue;
        }
        // Lazy continuation: a plain line directly under a quote belongs to the quote.
        if (!isBlank(lines[i]) && !startsBlock(lines, i)) {
          quoted.push(lines[i]);
          i += 1;
          continue;
        }
        break;
      }
      blocks.push({ type: 'blockquote', blocks: parseBlocks(quoted, depth + 1) });
      continue;
    }

    if (isListItemLine(line)) {
      const list = collectList(lines, i);
      blocks.push(list.block);
      i = list.next;
      continue;
    }

    const start = i;
    while (i < lines.length && !startsBlock(lines, i)) i += 1;
    blocks.push({ type: 'paragraph', lines: lines.slice(start, i) });
  }
  return blocks;
}

/** Split a table row on unescaped `|`, trimming the outer pipes and each cell. */
function splitRow(line) {
  let s = line.trim();
  if (s.startsWith('|')) s = s.slice(1);
  if (s.endsWith('|')) s = s.slice(0, -1);
  return s.split(/(?<!\\)\|/).map((cell) => cell.trim().replace(/\\\|/g, '|'));
}

function fitRow(cells, width) {
  const row = cells.slice(0, width);
  while (row.length < width) row.push('');
  return row;
}

function alignOf(cell) {
  const left = cell.startsWith(':');
  const right = cell.endsWith(':');
  if (left && right) return 'center';
  if (right) return 'right';
  if (left) return 'left';
  return '';
}

/**
 * One list level. Items may hold continuation lines (indented ≥ 2 spaces, or lazy unindented
 * lines) which are dedented and re-parsed recursively — that is how nested lists work.
 */
function collectList(lines, start) {
  const first = matchListItem(lines[start]);
  const base = first.indent;
  const ordered = first.ordered;
  const startNumber = first.number ?? 1;
  const items = [];
  let i = start;

  while (i < lines.length) {
    const item = matchListItem(lines[i]);
    if (!item || item.indent !== base || item.ordered !== ordered) break;

    const itemLines = [item.text];
    let checked = item.checked;
    i += 1;

    while (i < lines.length) {
      const line = lines[i];
      if (isBlank(line)) {
        const j = nextNonBlank(lines, i);
        if (j === -1) {
          i = lines.length;
          break;
        }
        const sibling = matchListItem(lines[j]);
        if (sibling && sibling.indent === base && sibling.ordered === ordered) {
          i = j;
          break;
        }
        if (indentOf(lines[j]) >= base + 2) {
          itemLines.push('');
          i += 1;
          continue;
        }
        i = j;
        break;
      }

      const sibling = matchListItem(line);
      if (sibling && sibling.indent === base && sibling.ordered === ordered) break;
      if (indentOf(line) >= base + 2) {
        itemLines.push(line.slice(base + 2));
        i += 1;
        continue;
      }
      if (startsBlock(lines, i)) break;
      itemLines.push(line); // lazy continuation
      i += 1;
    }

    items.push({
      checked,
      tight: !itemLines.some(isBlank),
      blocks: parseBlocks(itemLines),
    });
  }

  return { block: { type: 'list', ordered, start: startNumber, items }, next: i };
}

// ---------------------------------------------------------------------------
// 4. Block rendering
// ---------------------------------------------------------------------------

/**
 * Render escaped markdown to HTML. Pure: no `document`, no `window`, no I/O — importable from
 * plain `node --test`. The output is safe to assign to `innerHTML` (see the header).
 */
export function renderMarkdown(markdown) {
  const lines = normaliseLines(markdown);
  const blocks = parseBlocks(lines.map(escapeHtml));
  if (!blocks.length) return '';
  return renderBlocks(blocks, { slugs: new Set() });
}

function renderBlocks(blocks, ctx, opts = {}) {
  return blocks.map((b) => renderBlock(b, ctx, opts)).filter(Boolean).join('\n');
}

function renderBlock(block, ctx, opts) {
  switch (block.type) {
    case 'heading': {
      const id = uniqueSlug(slugify(block.text), ctx.slugs);
      return `<h${block.level} id="${id}">${renderInline(block.text)}</h${block.level}>`;
    }
    case 'paragraph': {
      const inner = renderParagraph(block.lines);
      return opts.tight ? inner : `<p>${inner}</p>`;
    }
    case 'hr':
      return '<hr>';
    case 'code': {
      const cls = block.lang ? ` class="language-${block.lang}"` : '';
      return `<pre><code${cls}>${block.text}</code></pre>`;
    }
    case 'blockquote':
      return `<blockquote>\n${renderBlocks(block.blocks, ctx)}\n</blockquote>`;
    case 'list':
      return renderList(block, ctx);
    case 'table':
      return renderTable(block);
    default:
      return '';
  }
}

function renderParagraph(lines) {
  const parts = lines.map((line) => {
    const hard = / {2,}$/.test(line);
    const text = hard ? line.replace(/ +$/, '') : line;
    return renderInline(text) + (hard ? '<br>' : '');
  });
  return parts.join('\n');
}

function renderList(block, ctx) {
  const tag = block.ordered ? 'ol' : 'ul';
  const attr = block.ordered && block.start !== 1 ? ` start="${block.start}"` : '';
  const items = block.items.map((item) => {
    const box = item.checked === null || item.checked === undefined
      ? ''
      : `<input type="checkbox" disabled${item.checked ? ' checked' : ''}> `;
    const inner = renderBlocks(item.blocks, ctx, { tight: item.tight });
    return `<li>${box}${inner}</li>`;
  });
  return `<${tag}${attr}>\n${items.join('\n')}\n</${tag}>`;
}

function renderTable(block) {
  const align = (k) => (block.align[k] ? ` style="text-align:${block.align[k]}"` : '');
  const head = block.header
    .map((cell, k) => `<th${align(k)}>${renderInline(cell)}</th>`)
    .join('');
  const rows = block.rows.map((row) => {
    const cells = row.map((cell, k) => `<td${align(k)}>${renderInline(cell)}</td>`).join('');
    return `<tr>${cells}</tr>`;
  }).join('\n');
  return `<table class="tbl">\n<thead>\n<tr>${head}</tr>\n</thead>\n<tbody>\n${rows}\n</tbody>\n</table>`;
}

// ---------------------------------------------------------------------------
// 5. Inline scanning
// ---------------------------------------------------------------------------

// Characters a backslash may escape (see the escaped-first rule: `<` arrives as `&lt;`).
const ESCAPABLE = '\\`*_{}[]()#+-.!|~<>';

// Characters that can begin an inline construct (or an entity this module produced).
const INLINE_SPECIAL = new Set(['\\', '`', '[', '&', '*', '_', '~']);

/**
 * Render inline markdown from *escaped* text. A left-to-right scanner, intentionally not a
 * regex tower: at each position it tries the constructs in priority order and otherwise copies
 * an already-escaped run of plain characters through.
 */
export function renderInline(text) {
  let out = '';
  let i = 0;
  while (i < text.length) {
    const ch = text[i];

    // Fast path: copy a run of ordinary text in one go (keeps the scan linear).
    if (!INLINE_SPECIAL.has(ch)) {
      let j = i + 1;
      while (j < text.length && !INLINE_SPECIAL.has(text[j])) j += 1;
      out += text.slice(i, j);
      i = j;
      continue;
    }

    const rest = text.slice(i);

    // Backslash escape: `\*` stays a literal `*`. `>` and `<` arrive here as `&gt;`/`&lt;`,
    // so a backslash in front of an entity is honoured too (`\>` → `&gt;`).
    if (ch === '\\' && i + 1 < text.length) {
      if (ESCAPABLE.includes(text[i + 1])) {
        out += text[i + 1];
        i += 2;
        continue;
      }
      const escaped = /^&(?:amp|lt|gt|quot|#39);/.exec(rest.slice(1));
      if (escaped) {
        out += escaped[0];
        i += 1 + escaped[0].length;
        continue;
      }
    }

    // Inline code span (backtick run, matched by an equal-length run).
    if (ch === '`') {
      const code = matchCodeSpan(rest);
      if (code) {
        out += `<code>${code.inner}</code>`;
        i += code.length;
        continue;
      }
    }

    // Autolink: `<https://…>` — in escaped text, `&lt;…&gt;`.
    if (text.startsWith('&lt;', i)) {
      const m = /^&lt;([^\s]{1,2048}?)&gt;/.exec(rest);
      if (m) {
        const href = sanitizeUrl(m[1]);
        if (href && /^(?:https?|mailto):/i.test(href)) {
          out += anchor(href, m[1]);
          i += m[0].length;
          continue;
        }
      }
    }

    // Inline link: `[text](url)`. Refused URLs fall through as literal text.
    if (ch === '[') {
      const m = /^\[([^\]]{0,512})\]\(([^)\s]{0,2048})\)/.exec(rest);
      if (m) {
        const href = sanitizeUrl(m[2]);
        if (href) {
          out += anchor(href, renderInline(m[1]));
          i += m[0].length;
          continue;
        }
        out += m[0]; // already escaped: plain text, never a link
        i += m[0].length;
        continue;
      }
    }

    // Emphasis: strong / strike / em, longest marker first.
    if (ch === '*' || ch === '_' || ch === '~') {
      const em = matchEmphasis(rest, (i > 0 ? text[i - 1] : ''));
      if (em) {
        out += `<${em.tag}>${renderInline(em.inner)}</${em.tag}>`;
        i += em.length;
        continue;
      }
    }

    out += ch;
    i += 1;
  }
  return out;
}

function matchCodeSpan(src) {
  let n = 0;
  while (src[n] === '`') n += 1;
  const marker = '`'.repeat(n);
  const close = src.indexOf(marker, n);
  if (close === -1) return null;
  let inner = src.slice(n, close);
  // A single leading+trailing space is the CommonMark way to write a code span starting/ending
  // with a backtick; it is trimmed.
  if (inner.length > 1 && inner.startsWith(' ') && inner.endsWith(' ') && inner.trim() !== '') {
    inner = inner.slice(1, -1);
  }
  return { inner, length: close + n };
}

const EMPHASIS = [
  { marker: '**', tag: 'strong' },
  { marker: '__', tag: 'strong' },
  { marker: '~~', tag: 'del' },
  { marker: '*', tag: 'em' },
  { marker: '_', tag: 'em' },
];

function matchEmphasis(src, prevChar) {
  for (const { marker, tag } of EMPHASIS) {
    if (!src.startsWith(marker)) continue;
    // `snake_case_names` must not italicise: `_` is only a marker at a word boundary.
    if (marker[0] === '_' && /[A-Za-z0-9]/.test(prevChar || '')) continue;
    const body = src.slice(marker.length);
    const end = body.indexOf(marker);
    if (end <= 0) continue;
    const inner = body.slice(0, end);
    if (/^\s/.test(inner) || /\s$/.test(inner)) continue;
    const after = body[end + marker.length] || '';
    if (marker[0] === '_' && /[A-Za-z0-9]/.test(after)) continue;
    return { tag, inner, length: marker.length * 2 + inner.length };
  }
  return null;
}

function anchor(href, label) {
  const external = /^https?:/i.test(href);
  return `<a href="${href}" rel="noreferrer noopener"${external ? ' target="_blank"' : ''}>${label}</a>`;
}

// ---------------------------------------------------------------------------
// 6. URL policy
// ---------------------------------------------------------------------------

const ALLOWED_SCHEMES = new Set(['http', 'https', 'mailto']);

/**
 * Allow only `http:`, `https:`, `mailto:` and relative/anchor hrefs. Everything else — notably
 * `javascript:`, `data:` and `vbscript:` — returns null so the caller renders plain text.
 * The argument is already escaped, so every `&` here is `&amp;` and no entity can smuggle a
 * colon past the scheme test.
 */
export function sanitizeUrl(rawUrl) {
  if (typeof rawUrl !== 'string') return null;
  let url = rawUrl.trim().replace(/[\u0000-\u001F\u007F]/g, '');
  if (!url) return null;
  if (url.startsWith('//')) return null; // protocol-relative → a host we cannot vouch for
  const m = /^([A-Za-z][A-Za-z0-9+.\-]*):/.exec(url);
  if (!m) return url; // relative path or #anchor
  return ALLOWED_SCHEMES.has(m[1].toLowerCase()) ? url : null;
}

// ---------------------------------------------------------------------------
// 7. Heading ids
// ---------------------------------------------------------------------------

/** `"## Install & Run"` → `"install-run"`; deterministic, never empty. */
export function slugify(text) {
  const plain = decodeEntities(String(text ?? ''))
    .replace(/<[^>]*>/g, '')
    .normalize('NFKD')
    .replace(/[\u0300-\u036f]/g, '')
    .toLowerCase()
    .replace(/[^a-z0-9\s_-]+/g, '')
    .trim()
    .replace(/[\s_]+/g, '-')
    .replace(/-+/g, '-')
    .replace(/^-+|-+$/g, '');
  return plain || 'section';
}

/** First use keeps the base; later clashes get `-2`, `-3`, … — never a duplicate id. */
function uniqueSlug(base, used) {
  let id = base;
  let n = 2;
  while (used.has(id)) {
    id = `${base}-${n}`;
    n += 1;
  }
  used.add(id);
  return id;
}
