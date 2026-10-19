// markdown.test.mjs — pure-unit tests for the docs renderer. No npm, no browser, no DOM.
//
//   node --test panel/test/markdown.test.mjs
//
// The security tests are the point: every one of them feeds hostile input and asserts that it
// comes back as visible text or is refused, never as live markup.

import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  renderMarkdown, renderInline, escapeHtml, sanitizeUrl, slugify,
} from '../markdown.js';

// ---------------------------------------------------------------------------
// escaping / XSS
// ---------------------------------------------------------------------------

test('a <script> tag is escaped, never markup', () => {
  const out = renderMarkdown('<script>alert(1)</script>');
  assert.equal(out, '<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>');
  assert.ok(!out.includes('<script'));
});

test('raw <b>x</b> renders as visible text', () => {
  const out = renderMarkdown('<b>x</b>');
  assert.ok(out.includes('&lt;b&gt;x&lt;/b&gt;'));
  assert.ok(!out.includes('<b>'));
});

test('all five significant characters are escaped', () => {
  assert.equal(escapeHtml(`&<>"'`), '&amp;&lt;&gt;&quot;&#39;');
  assert.equal(escapeHtml('&amp;'), '&amp;amp;'); // ampersand first: no double-decode
});

test('an HTML tag inside a paragraph cannot break out of the text', () => {
  const out = renderMarkdown('hello <img src=x onerror=alert(1)> world');
  assert.ok(!out.includes('<img'));
  assert.ok(out.includes('&lt;img'));
});

// ---------------------------------------------------------------------------
// links
// ---------------------------------------------------------------------------

test('[x](javascript:alert(1)) is not an anchor', () => {
  const out = renderMarkdown('[x](javascript:alert(1))');
  assert.ok(!out.includes('<a '));
  assert.ok(out.includes('[x](javascript:alert(1))'));
});

test('data:, vbscript: and protocol-relative URLs are refused', () => {
  for (const url of ['data:text/html,<script>1</script>', 'vbscript:msgbox(1)', '//evil.example/x']) {
    assert.equal(sanitizeUrl(url), null, url);
    assert.ok(!renderMarkdown(`[x](${url})`).includes('<a '), url);
  }
});

test('a normal https link is an anchor with rel and target', () => {
  const out = renderMarkdown('[docs](https://example.com/a?b=1&c=2)');
  assert.ok(out.includes('href="https://example.com/a?b=1&amp;c=2"'));
  assert.ok(out.includes('rel="noreferrer noopener"'));
  assert.ok(out.includes('target="_blank"'));
  assert.ok(out.includes('>docs</a>'));
});

test('relative, anchor and mailto links are allowed; mailto opens no new tab', () => {
  assert.ok(renderMarkdown('[a](./other.md)').includes('href="./other.md"'));
  assert.ok(renderMarkdown('[a](#install)').includes('href="#install"'));
  const mail = renderMarkdown('[mail](mailto:ops@example.com)');
  assert.ok(mail.includes('href="mailto:ops@example.com"'));
  assert.ok(mail.includes('rel="noreferrer noopener"'));
  assert.ok(!mail.includes('target="_blank"'));
});

test('a quote in a URL cannot break out of the href attribute', () => {
  const out = renderMarkdown('[x](https://example.com/?q="onmouseover=alert(1))');
  assert.ok(!out.includes('""'), out);
  assert.ok(out.includes('&quot;'));
});

test('bare autolinks <https://…> become links', () => {
  const out = renderMarkdown('see <https://example.com/x> now');
  assert.ok(out.includes('<a href="https://example.com/x" rel="noreferrer noopener" target="_blank">https://example.com/x</a>'));
});

test('an autolink with a hostile scheme stays text', () => {
  const out = renderMarkdown('<javascript:alert(1)>');
  assert.ok(!out.includes('<a '));
  assert.ok(out.includes('&lt;javascript:alert(1)&gt;'));
});

// ---------------------------------------------------------------------------
// headings
// ---------------------------------------------------------------------------

test('ATX headings get deterministic slug ids', () => {
  assert.ok(renderMarkdown('# Hello World').includes('<h1 id="hello-world">Hello World</h1>'));
  assert.ok(renderMarkdown('### Deep  Dive!').includes('<h3 id="deep-dive">'));
  assert.ok(renderMarkdown('# Install & Run').includes('id="install-run"'));
  assert.equal(slugify(''), 'section');
});

test('heading ids never collide', () => {
  const out = renderMarkdown('# Same\n\n# Same\n\n# Same');
  assert.ok(out.includes('id="same"'));
  assert.ok(out.includes('id="same-2"'));
  assert.ok(out.includes('id="same-3"'));
  assert.equal((out.match(/id="same"/g) || []).length, 1);
});

test('all six heading levels are supported', () => {
  for (let level = 1; level <= 6; level += 1) {
    assert.ok(renderMarkdown(`${'#'.repeat(level)} T`).includes(`<h${level} id="t">T</h${level}>`));
  }
  assert.ok(!renderMarkdown('####### seven').includes('<h7'));
});

// ---------------------------------------------------------------------------
// code
// ---------------------------------------------------------------------------

test('a fenced code block escapes < and preserves newlines', () => {
  const src = ['```js', 'const a = 1;', '<b>', '```'].join('\n');
  const out = renderMarkdown(src);
  assert.equal(out, '<pre><code class="language-js">const a = 1;\n&lt;b&gt;</code></pre>');
  assert.ok(!out.includes('<b>'));
});

test('~~~ fences work and take no highlighting', () => {
  const out = renderMarkdown(['~~~', 'x & y', '~~~'].join('\n'));
  assert.equal(out, '<pre><code>x &amp; y</code></pre>');
});

test('inline code containing * is not italicised', () => {
  const out = renderMarkdown('use `a*b*c` here');
  assert.ok(out.includes('<code>a*b*c</code>'));
  assert.ok(!out.includes('<em>'));
});

// ---------------------------------------------------------------------------
// inline formatting
// ---------------------------------------------------------------------------

test('bold, italic and strikethrough render', () => {
  assert.ok(renderMarkdown('**b**').includes('<strong>b</strong>'));
  assert.ok(renderMarkdown('__b__').includes('<strong>b</strong>'));
  assert.ok(renderMarkdown('*i*').includes('<em>i</em>'));
  assert.ok(renderMarkdown('_i_').includes('<em>i</em>'));
  assert.ok(renderMarkdown('~~gone~~').includes('<del>gone</del>'));
});

test('snake_case identifiers are not italicised', () => {
  const out = renderMarkdown('the some_long_name field');
  assert.ok(!out.includes('<em>'));
  assert.ok(out.includes('some_long_name'));
});

test('an escaped \\* stays literal', () => {
  const out = renderMarkdown('\\*not italic\\*');
  assert.equal(out, '<p>*not italic*</p>');
  assert.ok(!out.includes('<em>'));
});

test('hard line breaks need two trailing spaces', () => {
  assert.ok(renderMarkdown('one  \ntwo').includes('<br>'));
  assert.ok(!renderMarkdown('one\ntwo').includes('<br>'));
});

// ---------------------------------------------------------------------------
// tables
// ---------------------------------------------------------------------------

test('a pipe table renders <table> with per-column alignment', () => {
  const src = [
    '| Left | Center | Right |',
    '|:-----|:------:|------:|',
    '| a | b | c |',
  ].join('\n');
  const out = renderMarkdown(src);
  assert.ok(out.includes('<table class="tbl">'));
  assert.ok(out.includes('<th style="text-align:left">Left</th>'));
  assert.ok(out.includes('<th style="text-align:center">Center</th>'));
  assert.ok(out.includes('<th style="text-align:right">Right</th>'));
  assert.ok(out.includes('<td style="text-align:center">b</td>'));
  assert.ok(out.includes('<tbody>'));
});

test('table cells are inline-rendered and rows are padded to the header width', () => {
  const src = ['| a | b |', '|---|---|', '| **x** |', '| 1 | 2 | 3 |'].join('\n');
  const out = renderMarkdown(src);
  assert.ok(out.includes('<td><strong>x</strong></td>'));
  assert.ok(out.includes('<td>2</td>'));
  assert.ok(!out.includes('3'));
});

test('a bare --- stays a horizontal rule, not a table delimiter', () => {
  assert.equal(renderMarkdown('a | b\n---'), '<p>a | b</p>\n<hr>');
  assert.equal(renderMarkdown('---'), '<hr>');
});

test('a \\| inside a cell stays a literal pipe', () => {
  const src = ['| a | b |', '|---|---|', '| x \\| y | z |'].join('\n');
  const out = renderMarkdown(src);
  assert.ok(out.includes('<td>x | y</td>'));
});

// ---------------------------------------------------------------------------
// lists
// ---------------------------------------------------------------------------

test('unordered and ordered lists render', () => {
  assert.ok(renderMarkdown('- a\n- b').includes('<ul>\n<li>a</li>\n<li>b</li>\n</ul>'));
  assert.ok(renderMarkdown('1. a\n2. b').includes('<ol>\n<li>a</li>\n<li>b</li>\n</ol>'));
  assert.ok(renderMarkdown('3. c').includes('<ol start="3">'));
});

test('lists nest one level (and deeper)', () => {
  const out = renderMarkdown('- a\n  - b\n    - c');
  assert.equal((out.match(/<ul>/g) || []).length, 3);
  assert.ok(out.includes('<li>a'));
  assert.ok(out.includes('<li>b'));
  assert.ok(out.includes('<li>c</li>'));
});

test('an indented continuation line belongs to its item', () => {
  const out = renderMarkdown('- first line\n  second line\n- next');
  assert.ok(out.includes('<li>first line\nsecond line</li>'));
  assert.ok(out.includes('<li>next</li>'));
});

test('task-list checkboxes render as disabled inputs', () => {
  const out = renderMarkdown('- [x] done\n- [ ] todo');
  assert.ok(out.includes('<input type="checkbox" disabled checked>'));
  assert.ok(out.includes('<input type="checkbox" disabled>'));
  assert.ok(out.includes('done'));
  assert.ok(out.includes('todo'));
});

// ---------------------------------------------------------------------------
// blockquotes / rules / paragraphs
// ---------------------------------------------------------------------------

test('blockquotes nest and hold their own blocks', () => {
  const out = renderMarkdown(['> outer', '>', '> > inner', '>', '> back'].join('\n'));
  assert.equal((out.match(/<blockquote>/g) || []).length, 2);
  assert.ok(out.includes('<p>outer</p>'));
  assert.ok(out.includes('<p>inner</p>'));
  assert.ok(out.includes('<p>back</p>'));
});

test('an unmarked line lazily continues the open blockquote paragraph', () => {
  // CommonMark behaviour: `lazy` belongs to the quote, it does not start a new paragraph.
  assert.equal(renderMarkdown('> quoted\nlazy'), '<blockquote>\n<p>quoted\nlazy</p>\n</blockquote>');
});

test('paragraphs are separated by blank lines and joined when wrapped', () => {
  assert.equal(renderMarkdown('one\n\ntwo'), '<p>one</p>\n<p>two</p>');
  assert.equal(renderMarkdown('one\ntwo'), '<p>one\ntwo</p>');
});

// ---------------------------------------------------------------------------
// degenerate input
// ---------------------------------------------------------------------------

test('an empty or whitespace-only document returns an empty string without throwing', () => {
  assert.equal(renderMarkdown(''), '');
  assert.equal(renderMarkdown('   \n\n \t\n'), '');
  assert.equal(renderMarkdown(undefined), '');
  assert.equal(renderMarkdown(null), '');
  assert.equal(renderMarkdown('\r\n'), '');
  assert.equal(typeof renderMarkdown('x'), 'string');
});

test('an unclosed fence runs to the end of the document', () => {
  assert.equal(renderMarkdown('```\nno close'), '<pre><code>no close</code></pre>');
});

test('unmatched markers stay literal', () => {
  assert.equal(renderMarkdown('a * b'), '<p>a * b</p>');
  assert.equal(renderInline('**unclosed'), '**unclosed');
  assert.equal(renderInline('[not a link]'), '[not a link]');
});
